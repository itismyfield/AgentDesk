//! A selected Codex channel O committed confirms turn mode before its queue wakes: a refused
//! confirmation leaves the claim untouched and the reconcile tick wakes the queue exactly once.

use std::collections::HashMap;

use super::*;
use crate::services::discord::tui_direct_pending_start::channel_lock;
use crate::services::discord::turn_completion_events::{
    TurnCompletionEvent, subscribe_turn_completion_events,
};
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::turn_mode::{TestConfirmation, transcript_turns};

const CHANNEL: u64 = 64_700_601;

/// The channel committed on the home and selected for turn mode, as boot leaves it.
fn committed_and_selected() -> test_override::ChannelsGuard {
    let config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "w", "name": "W",
            "channels": {"codex": {"id": CHANNEL.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [CHANNEL]}, "turn": {"channels": [CHANNEL]}}
    }))
    .unwrap();
    let boot = BootChannels::validate(&config).unwrap();
    test_override::force_boot(boot.adopted(Adoption::Committed))
}

fn finished_turn() -> LedgerEntry {
    let mut completion_admission = CompletionAdmission::new(CompletionAdmissionPlan::Immediate);
    completion_admission.note_mailbox_released();
    LedgerEntry {
        recovery_lease: None,
        phase: Phase::Finalized,
        relay_owner: RelayOwnerKind::Watcher,
        provider: ProviderKind::Codex,
        turn_key: TurnKey::new(ChannelId::new(CHANNEL), 7, 0),
        terminal_deadline: None,
        watcher_backstop_deadline: None,
        watcher_backstop_probe_at: None,
        watcher_backstop_terminal_streak: 0,
        watcher_backstop_deadline_pulled: false,
        completion_admission,
        finalized_at: Some(Instant::now()),
    }
}

fn queue_wakes(events: &mut tokio::sync::broadcast::Receiver<TurnCompletionEvent>) -> usize {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| event.channel_id.get() == CHANNEL && event.queue_is_eligible())
        .count()
}

/// With the start lock held the claim stays unpublished and nothing wakes; once released, one
/// reconcile tick confirms turn mode and wakes the queue exactly once.
#[test]
fn a_refused_codex_confirmation_keeps_the_claim_for_one_reconcile_wake() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _boot = committed_and_selected();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _context = runtime.enter();
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    let mut events = subscribe_turn_completion_events(&shared);
    let entry = finished_turn();
    let key = entry.turn_key.exact_key();
    let mut ledger = HashMap::from([(key, entry)]);

    let lock = channel_lock("codex", CHANNEL);
    let held = lock.try_lock().unwrap();
    let entry = ledger.get_mut(&key).unwrap();
    assert!(!publish_claimed_queue_eligible(&shared, entry));
    assert!(!entry.completion_admission.queue_eligible_published);
    assert!(!transcript_turns(CHANNEL) && queue_wakes(&mut events) == 0);
    drop(held);

    runtime.block_on(super::super::reconcile::reconcile(
        &mut ledger,
        &mut HashMap::new(),
        &shared,
    ));
    let _confirmed = TestConfirmation::confirmed(CHANNEL);
    assert!(transcript_turns(CHANNEL));
    assert!(ledger[&key].completion_admission.queue_eligible_published);
    assert_eq!(queue_wakes(&mut events), 1);
    runtime.block_on(super::super::reconcile::reconcile(
        &mut ledger,
        &mut HashMap::new(),
        &shared,
    ));
    assert_eq!(
        queue_wakes(&mut events),
        0,
        "a published claim never wakes twice"
    );
}
