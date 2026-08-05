//! #5035 gate tests. Coordinates are relative to `origin/main` = `ac0c8feba`.
//!
//! Fixtures are merged per §3.4 lever 1 of the design note: several numbered
//! cases share one `SharedData` and one queue set-up, each on its own channel.
//! No assertion of any merged case was dropped.

use super::super::{PlaceholderEntrySlot, PlaceholderKey};
use super::*;
use crate::services::discord::{
    apply_queue_exit_feedback, gateway, make_shared_data_for_tests, queue_dispatch,
    queue_exit_drain_queued_placeholders, runtime_bootstrap,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{InterventionMode, QueueExitEvent};
use poise::serenity_prelude::UserId;

fn scoped_root() -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
    let root = tempfile::tempdir().expect("runtime root");
    let guard = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    (root, guard)
}

fn id(n: u64) -> MessageId {
    MessageId::new(500_035_000_000_000 + n)
}

fn chan(n: u64) -> ChannelId {
    ChannelId::new(500_035_900_000_000 + n)
}

fn queued(head: MessageId, sources: &[MessageId]) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: head,
        queued_generation: 1,
        source_message_ids: sources.to_vec(),
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("queued {}", head.get()),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// Enqueue through the real mailbox actor so `mailbox_snapshot` reads a genuine
/// `intervention_queue` (oldest→newest).
async fn seed(shared: &Arc<SharedData>, channel: ChannelId, items: Vec<Intervention>) {
    let ctx = queue_dispatch::persistence_context(shared, &ProviderKind::Claude, channel);
    for item in items {
        let outcome = shared.mailbox(channel).enqueue(item, ctx.clone()).await;
        assert!(outcome.enqueued, "fixture enqueue must succeed");
    }
}

fn card_of(shared: &SharedData, channel: ChannelId, owner: MessageId) -> Option<MessageId> {
    shared
        .queued
        .queued_placeholders
        .get(&(channel, owner))
        .map(|entry| *entry)
}

fn map_entries(shared: &SharedData, channel: ChannelId) -> Vec<(u64, u64)> {
    let mut entries: Vec<(u64, u64)> = shared
        .queued
        .queued_placeholders
        .iter()
        .filter(|entry| entry.key().0 == channel)
        .map(|entry| (entry.key().1.get(), entry.value().get()))
        .collect();
    entries.sort_unstable();
    entries
}

fn exit(intervention: Intervention, kind: QueueExitKind) -> QueueExitEvent {
    QueueExitEvent { intervention, kind }
}

fn owner_of(disposition: QueuedCardDisposition, what: &str) -> MessageId {
    match disposition {
        QueuedCardDisposition::Preserved { owner } => owner,
        QueuedCardDisposition::Released(_) => panic!("{what} must not be released"),
    }
}

/// T1 (A1/A2 + departing preference), T1b (a candidate named in `D` is still a
/// candidate), T2 (r5 leg A, SAFETY ONLY — the recipient preference lives in
/// `t2b_…`), T5 + T5b (the gate must still release, and the teardown must drop
/// the controller row — via the helper and via the production exit-body branch)
/// and T8 (duplicate map values: only G1 can see the surviving owner).
#[tokio::test]
async fn t1_t1b_t2_t5_t8_queue_exit_verdicts_over_the_whole_queue() {
    let _root = scoped_root();
    let shared = make_shared_data_for_tests();

    // T1 — `B` owns the card and leaves; `A` is still queued and owns nothing.
    let (c1, a, b, x) = (chan(1), id(1), id(2), id(90));
    seed(&shared, c1, vec![queued(a, &[a]), queued(b, &[b])]).await;
    shared.queued.queued_placeholders.insert((c1, b), x);
    let event = exit(queued(b, &[b]), QueueExitKind::Overflow);
    let released = queue_exit_drain_queued_placeholders(&shared, c1, &[&event]).await;
    assert!(released.is_empty(), "T1: X must issue no teardown token");
    assert_eq!(card_of(&shared, c1, a), Some(x), "T1: X re-keys onto A");
    assert_eq!(card_of(&shared, c1, b), None, "T1: B loses the mapping");

    // T1b — the ONLY candidate is itself named in the departing hint. Excluding
    // it would turn the hint back into a destructive verdict.
    let (c2, x2) = (chan(2), id(91));
    seed(&shared, c2, vec![queued(b, &[b])]).await;
    let owner = owner_of(release_or_rekey(&shared, c2, x2, &[b]).await, "T1b: X2");
    assert_eq!(
        owner, b,
        "T1b: a departing candidate still receives the card"
    );
    assert_eq!(card_of(&shared, c2, b), Some(x2));

    // T2 — r5 leg A: two cardless entries, no departing hint at all. This block
    // asserts the SAFETY property only: the card must survive and land on SOME
    // live queue entry that held no card. WHICH entry is a hop-count preference,
    // not a safety requirement (contract G is indifferent — see the gate module
    // docs), so every assertion here must keep passing if the preference is
    // reversed. The preference itself is pinned separately in
    // `t2b_recipient_preference_is_a_hop_optimisation_not_a_safety_property`.
    let (c3, p) = (chan(3), id(93));
    seed(&shared, c3, vec![queued(a, &[a]), queued(b, &[b])]).await;
    let cardless_before: Vec<MessageId> = [a, b]
        .into_iter()
        .filter(|entry| card_of(&shared, c3, *entry).is_none())
        .collect();
    assert_eq!(
        cardless_before.len(),
        2,
        "T2 fixture: both entries cardless"
    );
    let owner = owner_of(release_or_rekey(&shared, c3, p, &[]).await, "T2: P");
    assert!(
        cardless_before.contains(&owner),
        "T2: P must be re-keyed onto a live queue entry that held no card"
    );
    assert_eq!(card_of(&shared, c3, owner), Some(p), "T2: recipient owns P");

    // T8 — two keys point at the same card and the departing key is drained
    // first, so only G1 can see that `A` still owns it.
    let (c4, s4) = (chan(4), id(41));
    seed(&shared, c4, vec![queued(a, &[a])]).await;
    shared.queued.queued_placeholders.insert((c4, a), x);
    shared.queued.queued_placeholders.insert((c4, s4), x);
    let event4 = exit(queued(s4, &[s4]), QueueExitKind::Cancelled);
    let released4 = queue_exit_drain_queued_placeholders(&shared, c4, &[&event4]).await;
    assert!(released4.is_empty(), "T8: X still belongs to the queued A");
    assert_eq!(
        map_entries(&shared, c4),
        vec![(a.get(), x.get())],
        "T8: no key added or removed besides the drained one"
    );

    // T5 — an empty queue must still release, and the teardown must drop the
    // controller row (regression guard: the gate must not become a no-op).
    let (c5, s5, x5) = (chan(5), id(5), id(92));
    shared.queued.queued_placeholders.insert((c5, s5), x5);
    shared.ui.placeholder_controller.entries.insert(
        PlaceholderKey {
            provider: ProviderKind::Claude,
            channel_id: c5,
            message_id: x5,
        },
        Arc::new(PlaceholderEntrySlot::default()),
    );
    let event5 = exit(queued(s5, &[s5]), QueueExitKind::Cancelled);
    let mut released5 = queue_exit_drain_queued_placeholders(&shared, c5, &[&event5]).await;
    assert_eq!(released5.len(), 1, "T5: an empty queue releases the card");
    let (_, teardown) = released5.pop().expect("one released card");
    assert_eq!(teardown.card(), x5);
    assert_eq!(teardown_defer(&shared, teardown), x5);
    assert!(
        shared.ui.placeholder_controller.entries.is_empty(),
        "T5: teardown must drop the controller row"
    );

    // T5b — the same controller-row evidence, but through the PRODUCTION
    // exit-body branch of `apply_queue_exit_feedback`. Every teardown helper
    // ends in `detach()`, so a controller row that outlives a `Released` verdict
    // proves no teardown ran. This is the only assertion outside the T7 ratchet
    // that can see the exit-body site at all (no Discord HTTP boundary is
    // available — `serenity_http_or_token_fallback` builds its own client).
    let (c6, s6, x6) = (chan(6), id(6), id(89));
    let _ = shared
        .http
        .cached_bot_token
        .set("Bot test-token".to_string());
    shared.queued.queued_placeholders.insert((c6, s6), x6);
    let row = PlaceholderKey {
        provider: ProviderKind::Claude,
        channel_id: c6,
        message_id: x6,
    };
    shared
        .ui
        .placeholder_controller
        .entries
        .insert(row.clone(), Arc::new(PlaceholderEntrySlot::default()));
    apply_queue_exit_feedback(
        &shared,
        c6,
        &[exit(queued(s6, &[s6]), QueueExitKind::Cancelled)],
    )
    .await;
    assert!(
        shared.ui.placeholder_controller.entries.get(&row).is_none(),
        "T5b: the exit-body branch must consume the token, which detaches the row"
    );
}

/// T2b — PREFERENCE, NOT SAFETY.
///
/// The gate hands a re-keyed card to the NEWEST cardless candidate. That is a
/// hop-count optimisation (the producer re-keys toward the newest arrival while
/// the dispatcher consumes oldest-first), NOT a requirement of contract G:
/// oldest is equally safe, because the verdict is `Preserved` either way and a
/// card re-keyed onto the oldest entry is simply re-gated on the next dispatch.
///
/// If the preference is ever deliberately changed, THIS is the test to update —
/// and the safety assertions in
/// `t1_t1b_t2_t5_t8_queue_exit_verdicts_over_the_whole_queue` must keep passing
/// untouched. A change that needs both edited is not a preference change.
#[tokio::test]
async fn t2b_recipient_preference_is_a_hop_optimisation_not_a_safety_property() {
    let _root = scoped_root();
    let shared = make_shared_data_for_tests();
    let (c, a, b, p) = (chan(11), id(11), id(12), id(83));
    seed(&shared, c, vec![queued(a, &[a]), queued(b, &[b])]).await;
    let owner = owner_of(release_or_rekey(&shared, c, p, &[]).await, "T2b: P");
    assert_eq!(owner, b, "T2b: newest cardless candidate is preferred");
    assert_eq!(card_of(&shared, c, b), Some(p));
}

/// T3 — r5 leg B: the newest survivor already owns a different card, so the
/// candidate set must not collapse to "the newest head".
#[tokio::test]
async fn t3_dequeued_card_goes_to_the_cardless_survivor_not_the_carded_newest() {
    let _root = scoped_root();
    let shared = make_shared_data_for_tests();
    let (c, h, a, d) = (chan(20), id(20), id(21), id(22));
    let (x, y) = (id(94), id(95));
    seed(
        &shared,
        c,
        vec![queued(h, &[h]), queued(a, &[a]), queued(d, &[d])],
    )
    .await;
    shared.queued.queued_placeholders.insert((c, h), x);
    shared.queued.queued_placeholders.insert((c, d), y);

    let ctx = queue_dispatch::persistence_context(&shared, &ProviderKind::Claude, c);
    let taken = shared.mailbox(c).take_next_soft(ctx).await;
    let head = taken.intervention.map(|item| item.message_id);
    assert_eq!(head, Some(h), "dispatch consumes oldest-first");
    assert_eq!(shared.remove_queued_placeholder(c, h).await, Some(x));

    let owner = owner_of(release_or_rekey(&shared, c, x, &[h]).await, "T3: X");
    assert_eq!(owner, a, "T3: the cardless survivor A takes X");
    assert_eq!(card_of(&shared, c, a), Some(x));
    assert_eq!(card_of(&shared, c, d), Some(y), "T3: Y must be untouched");
}

/// T4a — the survivor owns its card through a NON-head source id, so a
/// head-only ownership test mis-reads it as cardless. T4b (step two) is the
/// preservation direction on the same fixture.
#[tokio::test]
async fn t4_merged_drain_releases_only_when_every_survivor_still_holds_a_card() {
    let _root = scoped_root();
    let shared = make_shared_data_for_tests();
    let (c, s, a_head) = (chan(30), id(30), id(31));
    let (n, m_head, x, y) = (id(32), id(33), id(96), id(101));
    seed(&shared, c, vec![queued(a_head, &[s, a_head])]).await;
    shared.queued.queued_placeholders.insert((c, s), y);
    shared.queued.queued_placeholders.insert((c, n), x);

    let drained = gateway::drain_merged_queued_placeholders(&shared, c, m_head, &[n, m_head]).await;
    assert_eq!(drained.len(), 1, "T4a: no survivor is cardless — release X");
    assert_eq!(drained[0].card(), x);
    assert_eq!(
        card_of(&shared, c, a_head),
        None,
        "T4a: no re-key onto a survivor that already holds a card"
    );
    assert_eq!(card_of(&shared, c, s), Some(y), "T4a: Y must be untouched");

    // T4b — make the survivor genuinely cardless, then drain a second merged
    // exit: the same helper must now preserve.
    shared.queued.queued_placeholders.remove(&(c, s));
    let (n2, m2_head, x2) = (id(34), id(35), id(97));
    shared.queued.queued_placeholders.insert((c, n2), x2);
    let drained2 =
        gateway::drain_merged_queued_placeholders(&shared, c, m2_head, &[n2, m2_head]).await;
    assert!(drained2.is_empty(), "T4b: X2 is preserved for the survivor");
    assert_eq!(card_of(&shared, c, a_head), Some(x2));
}

struct RecordingDeleter(std::sync::Mutex<Vec<u64>>);

impl runtime_bootstrap::StalePlaceholderDeleter for RecordingDeleter {
    fn delete<'a>(
        &'a self,
        _channel_id: ChannelId,
        placeholder_msg_id: MessageId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(placeholder_msg_id.get());
            Ok(())
        })
    }
}

/// T6 — bootstrap: a stale OWNER does not make the card unowned while the same
/// channel still has a live, cardless queue entry. Observed through the
/// existing `StalePlaceholderDeleter` seam, so "DELETE not issued" is literal.
#[tokio::test]
async fn t6_bootstrap_stale_card_is_rekeyed_not_deleted_when_a_live_entry_is_cardless() {
    let _root = scoped_root();
    let shared = make_shared_data_for_tests();
    let (c, a, stale_owner, x) = (chan(40), id(40), id(42), id(98));
    seed(&shared, c, vec![queued(a, &[a])]).await;
    let deleter = RecordingDeleter(std::sync::Mutex::new(Vec::new()));

    runtime_bootstrap::delete_stale_queued_placeholder_cards_with(
        &deleter,
        &shared,
        &[(c, stale_owner, x)],
    )
    .await;

    let calls = deleter
        .0
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    assert!(calls.is_empty(), "T6: no DELETE while A is queued cardless");
    assert_eq!(card_of(&shared, c, a), Some(x), "T6: X re-keys onto A");
}

/// Assert that `op` mutates no `queued_placeholders` row of `channel` and does
/// not complete while the test holds that channel's persist lock. The kill
/// signal is the STATE assertion: a raw `DashMap::remove` that escapes the
/// outer lock changes the map before reaching any suspension point.
///
/// NOT deterministic. It assumes `#[tokio::test]`'s default current-thread
/// scheduler, but Tokio does not guarantee poll order — it may re-poll the
/// yielding task ahead of the spawned one
/// (`tokio-1.50.0/src/task/yield_now.rs:13`). The kill is an EMPIRICAL
/// observation: a review probe on Tokio 1.50.0 ran 100,000 iterations with
/// `missed_after_one=0 missed_after_64=0`. The 64-turn budget is that empirical
/// mitigation, not an API guarantee, so the lock mutants are killed
/// PROBABILISTICALLY and a false negative remains possible. Rewriting these as
/// `flavor = "multi_thread"` weakens the observation further; nothing enforces
/// the flavor.
async fn assert_waits_for_persist_lock<F, Fut, T>(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    op: F,
) -> T
where
    F: FnOnce(Arc<SharedData>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let lock = shared.queued_placeholders_persist_lock(channel);
    let guard = lock.lock().await;
    let before = map_entries(shared, channel);
    let handle = tokio::spawn(op(shared.clone()));
    for turn in 0..64 {
        tokio::task::yield_now().await;
        assert_eq!(
            map_entries(shared, channel),
            before,
            "queued_placeholders mutated at scheduler turn {turn} under the persist lock — a raw remove escaped the outer lock"
        );
        assert!(
            !handle.is_finished(),
            "gated op completed at scheduler turn {turn} without the persist lock"
        );
    }
    drop(guard);
    handle.await.expect("gated op panicked")
}

/// T9 — `release_or_rekey` takes the lock itself (this channel has no mailbox,
/// so `mailbox_snapshot` returns the default without awaiting — `mod.rs:1195`
/// + `mod.rs:1477` — leaving the lock as the only suspension point).
/// T10a/T10b — the raw `DashMap::remove` in the A1/A2 and A4/A5 helpers must
/// stay behind the OUTER lock. Both fixtures make the correct code mutate the
/// map, so the state assertion has a live control group.
#[tokio::test]
async fn t9_t10a_t10b_gated_helpers_wait_for_the_persist_lock() {
    let _root = scoped_root();
    let shared = make_shared_data_for_tests();

    let (c9, d, x9) = (chan(60), id(60), id(80));
    let disposition = assert_waits_for_persist_lock(&shared, c9, move |shared| async move {
        release_or_rekey(&shared, c9, x9, &[d]).await
    })
    .await;
    assert!(matches!(
        disposition,
        QueuedCardDisposition::Released(ref teardown) if teardown.card() == x9
    ));

    let (c10, b, x10) = (chan(61), id(61), id(81));
    shared.queued.queued_placeholders.insert((c10, b), x10);
    let released = assert_waits_for_persist_lock(&shared, c10, move |shared| async move {
        let event = exit(queued(b, &[b]), QueueExitKind::Cancelled);
        queue_exit_drain_queued_placeholders(&shared, c10, &[&event]).await
    })
    .await;
    assert_eq!(released.len(), 1, "T10a: the drain still completes");
    assert_eq!(card_of(&shared, c10, b), None);

    let (c11, n, head, x11) = (chan(62), id(62), id(63), id(82));
    shared.queued.queued_placeholders.insert((c11, n), x11);
    let drained = assert_waits_for_persist_lock(&shared, c11, move |shared| async move {
        gateway::drain_merged_queued_placeholders(&shared, c11, head, &[n, head]).await
    })
    .await;
    assert_eq!(drained.len(), 1, "T10b: the drain still completes");
    assert_eq!(card_of(&shared, c11, n), None);
}

fn region<'a>(source: &'a str, label: &str, start: &str, end: &str) -> &'a str {
    let from = source
        .find(start)
        .unwrap_or_else(|| panic!("{label}: start marker missing ({start})"));
    let len = source[from..]
        .find(end)
        .unwrap_or_else(|| panic!("{label}: end marker missing ({end})"));
    &source[from..from + len]
}

/// Every spelling by which a gated region could reach Discord's message DELETE
/// or PATCH without a teardown token.
///
/// Enumerated from the tree, NOT from what the pre-#5035 code happened to call:
/// `git grep -ohE "[A-Za-z_.:]*(delete|edit)_[a-z_]*message[a-z_]*\(" --
/// 'src/services/discord/**/*.rs' | sort -u`. Both the serenity spellings
/// (`.delete_message(`, `.edit_message(`) and the in-tree helper spellings
/// (`http::delete_channel_message`, `http::edit_channel_message`, the
/// `_with_components` and `edit_outbound_message*` variants) are covered; the
/// earlier two-string list missed every helper spelling in both directions.
///
/// This list is deliberately NOT applied to `queued_card_gate.rs` itself, which
/// legitimately calls `http::delete_channel_message` / `http::edit_channel_message`
/// inside its teardown helpers. The gate file is not among the regions below.
const RAW_DISCORD_MESSAGE_OPS: &[&str] = &[
    "delete_message(",
    "delete_channel_message(",
    "delete_rollback_channel_message(",
    "edit_message(",
    "edit_channel_message(",
    "edit_channel_message_with_components(",
    "edit_outbound_message(",
    "edit_outbound_message_classified(",
];

/// Both conditions must hold INDEPENDENTLY: the region routes through the gate,
/// AND it holds no raw destructive call. A region containing a gate call *and* a
/// raw call fails the second assertion — co-existence is rejected, not accepted.
fn gated(label: &str, source: &str, start: &str, end: &str) {
    let body = region(source, label, start, end);
    assert!(body.contains("queued_card_gate::"), "{label}: not gated");
    for op in RAW_DISCORD_MESSAGE_OPS {
        assert!(
            !body.contains(op),
            "{label}: raw `{op}` inside a gated region"
        );
    }
}

/// T7 — source-text ratchet over a FIXED region list: it asserts wiring only. A
/// raw destructive call in a file that is not listed, or in an unlisted region
/// of a listed file, passes.
#[test]
fn t7_gated_sites_hold_no_raw_destructive_call() {
    const MOD_RS: &str = include_str!("../../mod.rs");
    const GATEWAY_RS: &str = include_str!("../../gateway.rs");
    const INTAKE_RS: &str = include_str!("../../router/message_handler/intake_turn.rs");
    const HANDOFF_RS: &str =
        include_str!("../../router/message_handler/intake_turn/placeholder_handoff.rs");
    const BOOTSTRAP_RS: &str = include_str!("../../runtime_bootstrap/queued_placeholders.rs");
    const FLUSH_RS: &str = include_str!("../../runtime_bootstrap/recovery_flush.rs");
    const CONTROLLER_RS: &str = include_str!("../../placeholder_controller.rs");

    gated(
        "A1/A2 queue-exit drain",
        MOD_RS,
        "async fn queue_exit_drain_queued_placeholders",
        "struct QueueExitPendingPlaceholderDeleter",
    );
    gated(
        "A3 deferred drain",
        MOD_RS,
        "async fn drain_pending_queue_exit_placeholder_clears_with",
        "async fn enqueue_internal_followup",
    );
    gated(
        "A5 kickoff drain",
        MOD_RS,
        "let drained_cards = gateway::drain_merged_queued_placeholders(",
        "let dispatch_result =",
    );
    gated(
        "A4 merged drain helper",
        GATEWAY_RS,
        "pub(super) async fn drain_merged_queued_placeholders",
        "fn live_bot_owner_provider",
    );
    gated(
        "A4 dispatch call site",
        GATEWAY_RS,
        "let drained = drain_merged_queued_placeholders(",
        "router::finish_admitted_queued_intake",
    );
    gated(
        "A6 dequeue hand-off",
        INTAKE_RS,
        "// Fresh anchor is live; tear down the buried queued card.",
        "📬➡️🔄 DISPATCH: queued dequeued",
    );
    gated(
        "A7 busy-notice reuse",
        HANDOFF_RS,
        "if let Some(stale_queued) = queued_placeholder_handoff",
        "routed the queued card through the #5035 gate",
    );
    gated(
        "A8 stale-card delete",
        BOOTSTRAP_RS,
        "async fn delete_stale_queued_placeholder_cards_with",
        "async fn collect_live_queue_message_ids",
    );

    // The A8 bootstrap caller does not name the gate itself — it must hand
    // `shared` to the helper that re-gates each card.
    let flush = region(
        FLUSH_RS,
        "A8 bootstrap caller",
        "delete_stale_queued_placeholder_cards(",
        "mark_reconcile_complete",
    );
    assert!(
        flush.contains("&shared_for_tmux2"),
        "A8 caller: no `shared`"
    );
    for op in RAW_DISCORD_MESSAGE_OPS {
        assert!(!flush.contains(op), "A8 caller: raw `{op}`");
    }
    assert!(
        !CONTROLLER_RS.contains(".detach_by_message("),
        "placeholder_controller.rs itself must hold no detach_by_message call site"
    );
}
