//! A selected Codex channel O committed confirms turn mode before its queue wakes, on the source its
//! takeover committed; a refused confirmation keeps the claim, past the TTL, for one reconcile wake.

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

/// The late-adopt boot snapshot after this process took the channel over, or after a restart.
fn taken_over(restarted: bool) -> test_override::ChannelsGuard {
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "w", "name": "W",
            "channels": {"codex": {"id": CHANNEL.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [CHANNEL], "late_adopt": true},
            "turn": {"channels": [CHANNEL]}}
    }))
    .unwrap();
    let state = [Adoption::Pending, Adoption::Committed][usize::from(restarted)];
    let seeded = |_: &_, _| Ok(std::collections::BTreeMap::from([(CHANNEL, state)]));
    let boot = BootChannels::validate(&config).unwrap();
    let boot = boot.seeded(true, &config, seeded).unwrap();
    let candidate = boot.candidate(CHANNEL).unwrap();
    assert_eq!(candidate.late_adopt_origin(), !restarted);
    *candidate.lock() = Adoption::Committed;
    test_override::force_boot(boot)
}

fn line(value: serde_json::Value) -> String {
    format!("{value}\n")
}

/// A rollout bound through the hook log; `turn` opens its first turn after the metadata.
fn bound(
    root: &std::path::Path,
    name: &str,
    turn: bool,
) -> crate::services::tui_o::shadow::SourceId {
    let path = root.join(name);
    let mut text = line(serde_json::json!({"type": "session_meta",
        "payload": {"id": "parent", "cwd": root, "source": "cli"}}));
    if turn {
        text += &line(serde_json::json!({"type": "event_msg",
            "payload": {"type": "task_started", "turn_id": "t1"}}));
        text += &line(
            serde_json::json!({"type": "response_item", "payload": {"type": "message",
            "role": "user", "content": [{"type": "input_text", "text": "hi"}]}}),
        );
    }
    std::fs::write(&path, text).unwrap();
    use crate::services::tui_prompt_dedupe::binding_events as p5;
    let hook =
        p5::HookSignal::from_payload("session_start", &serde_json::json!({"source": "startup"}));
    let proposal = p5::Proposal {
        channel_id: CHANNEL,
        provider: "codex",
        tmux_session: "tmux-codex",
        session_id: Some("parent"),
        path: path.to_str().unwrap(),
        replaced: None,
        cause: p5::CauseSource::Hook(hook.cause()),
        hook: Some(&hook),
    };
    assert_eq!(
        p5::record_source(&proposal).unwrap(),
        p5::Committed::Appended
    );
    crate::services::tui_o::shadow::binding_reader::source_id_for("parent", &path).unwrap()
}

fn end_turn(source: &crate::services::tui_o::shadow::SourceId) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.path)
        .unwrap();
    file.write_all(
        line(
            serde_json::json!({"type": "response_item", "payload": {"type": "message",
        "role": "assistant", "id": "m1", "content": [{"type": "output_text", "text": "done"}]}}),
        )
        .as_bytes(),
    )
    .unwrap();
    file.write_all(
        line(serde_json::json!({"type": "event_msg",
        "payload": {"type": "task_complete", "turn_id": "t1"}}))
        .as_bytes(),
    )
    .unwrap();
}

/// O took over rollout A, whose turn still runs, and the binding log then rotated to a
/// metadata-only B.
fn committed_a_then_b(root: &std::path::Path) -> crate::services::tui_o::shadow::SourceId {
    use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
    crate::services::tui_prompt_dedupe::binding_events::set_test_root(Some(root));
    let a = bound(root, "rollout-a.jsonl", true);
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root);
    let init = Initialized {
        channel: CHANNEL,
        sources: vec![InitSource {
            source_id: a.clone(),
            delivery_start: 0,
            prefix_hash: String::new(),
        }],
        initial_anchor: 0,
        build_digest: "test".into(),
        at: chrono::Utc::now(),
    };
    store.unwrap().unwrap().init_channel(&init).unwrap();
    bound(root, "rollout-b.jsonl", false);
    a
}

fn reconciled(
    runtime: &tokio::runtime::Runtime,
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    shared: &std::sync::Arc<crate::services::discord::SharedData>,
) {
    let mut pending = HashMap::new();
    let pass = super::super::reconcile::reconcile(ledger, &mut pending, shared);
    runtime.block_on(pass);
}

/// After the binding rotates to a metadata-only B, the takeover confirms once A's turn ends and the
/// queue wakes exactly once; an expired claim is kept while it waits, then collected.
#[test]
fn a_takeover_confirms_on_its_committed_source_after_the_binding_rotates() {
    for expired in [false, true] {
        let root = crate::config::TestRuntimeRootGuard::new();
        let runtime_root = crate::config::runtime_root().unwrap();
        let _boot = taken_over(false);
        let a = committed_a_then_b(&runtime_root);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _context = runtime.enter();
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let mut events = subscribe_turn_completion_events(&shared);
        let mut entry = finished_turn();
        if expired {
            let age = super::super::COMPLETION_ADMISSION_TTL + std::time::Duration::from_secs(1);
            entry.finalized_at = Instant::now().checked_sub(age);
        }
        let key = entry.turn_key.exact_key();
        let mut ledger = HashMap::from([(key, entry)]);

        reconciled(&runtime, &mut ledger, &shared);
        assert!(
            ledger.contains_key(&key),
            "expired={expired}: the claim is kept"
        );
        assert!(!transcript_turns(CHANNEL) && queue_wakes(&mut events) == 0);
        end_turn(&a);
        reconciled(&runtime, &mut ledger, &shared);
        let _confirmed = TestConfirmation::confirmed(CHANNEL);
        assert!(transcript_turns(CHANNEL), "expired={expired}");
        assert_eq!(queue_wakes(&mut events), 1);
        assert_eq!(
            ledger.contains_key(&key),
            !expired,
            "collected once confirmed"
        );
        crate::services::tui_prompt_dedupe::binding_events::set_test_root(None);
        drop(root);
    }
}

/// With no confirmation in memory, a restarted late-adopt boot reads the takeover from its init:
/// A's open turn holds turn mode though B is bound, and A's end confirms it.
#[test]
fn a_restarted_takeover_confirms_on_its_init_source() {
    use super::super::super::tui_direct_pending_start::turn_retirement::codex;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime_root = crate::config::runtime_root().unwrap();
    let _boot = taken_over(true);
    let a = committed_a_then_b(&runtime_root);
    let turn = crate::services::tui_o::cutover::codex_turn_awaited(CHANNEL).unwrap();
    assert!(matches!(
        codex::confirm_current(CHANNEL, &turn),
        codex::Confirmation::Retry(_)
    ));
    end_turn(&a);
    assert_eq!(
        codex::confirm_current(CHANNEL, &turn),
        codex::Confirmation::Confirmed
    );
    let _confirmed = TestConfirmation::confirmed(CHANNEL);
    crate::services::tui_prompt_dedupe::binding_events::set_test_root(None);
}
