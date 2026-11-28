use std::sync::Arc;

use crate::services::discord::gateway::{DiscordGateway, TurnGateway};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::Intervention;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

/// #4270 — env-lock + temp runtime root held in a struct so no MutexGuard
/// binding lives across `.await` (keeps the `await_holding_lock` allow
/// ratchet flat; same pattern as `queue_marker.rs::ScopedRuntimeRoot`).
struct ScopedRuntimeRoot {
    _lock: std::sync::MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
}

impl Drop for ScopedRuntimeRoot {
    fn drop(&mut self) {
        unsafe {
            match self.prev.take() {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }
}

fn scoped_runtime_root() -> ScopedRuntimeRoot {
    let lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
    let temp = tempfile::tempdir().expect("temp runtime root");
    unsafe {
        std::env::set_var(
            "AGENTDESK_ROOT_DIR",
            temp.path().to_str().expect("temp path must be valid utf-8"),
        );
    }
    ScopedRuntimeRoot {
        _lock: lock,
        _temp: temp,
        prev,
    }
}

fn promoted_intervention(id: u64, text: &str) -> Intervention {
    Intervention {
        author_id: UserId::new(id),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: text.to_string(),
        mode: crate::services::turn_orchestrator::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

fn turn_view_persisted_path(channel_id: ChannelId, message_id: MessageId) -> std::path::PathBuf {
    crate::services::discord::runtime_store::discord_turn_view_reconciler_root()
        .expect("turn view reconciler root")
        .join("intake_user_message")
        .join(format!("{}-{}.json", channel_id.get(), message_id.get()))
}

/// #4270 pin (entrypoint, busy) — the promote gate at the head of
/// `DiscordGateway::dispatch_queued_turn` defers a hosted-TUI-busy queued
/// turn BEFORE any teardown, SILENTLY, across repeated retry cycles: each
/// cycle the intervention goes back to the queue front with the dispatch
/// reservation consumed, the mailbox is never claimed, the slow (60s)
/// fail-open backstop stays coalesced to ONE, and — the user-visible
/// invariant — the head's `📬 Queued` turn-view state survives byte-for-byte
/// with ZERO reconciler reaction ops (no `⏳` flip, no `📬` drain, no merged
/// queued-card deletion) and no notice/card accumulation: the defer branch
/// contains no Discord send/edit/post call site at all (user report: the
/// old spin posted a fresh `📬 재시도 큐 등록` card every cycle).
#[tokio::test(flavor = "current_thread")]
async fn dispatch_promote_gate_busy_defers_silently_across_retry_cycles_4270() {
    let _root = scoped_runtime_root();
    let _busy = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(true);

    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    shared
        .turn_view_reconciler
        .record_test_reactions(http.clone());
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(100_000_004_270_600);
    let user_msg = MessageId::new(100_000_004_270_601);

    // Seed the promoted follow-up's user-visible queued view: 📬 marker via
    // the reconciler (persisted Queued state + one recorded reaction op).
    crate::services::discord::turn_view_reconciler::note_intake_queue_marker_added_current(
        &shared,
        &http,
        channel_id,
        user_msg,
        crate::services::discord::queue_reactions::QUEUE_STANDALONE_PENDING_REACTION,
        "test_seed_queued_view",
    )
    .await;
    let view_path = turn_view_persisted_path(channel_id, user_msg);
    assert!(view_path.exists(), "seeded 📬 queued view must persist");
    let view_before = std::fs::read(&view_path).expect("read seeded turn view state");
    let ops_before = shared.turn_view_reconciler.ops().len();

    let persistence =
        crate::services::discord::queue_persistence_context(&shared, &provider, channel_id);
    shared
        .mailbox(channel_id)
        .replace_queue(
            vec![promoted_intervention(user_msg.get(), "busy promote head")],
            persistence.clone(),
        )
        .await;

    let gateway = DiscordGateway::new(http.clone(), shared.clone(), provider.clone(), None);
    // Three watcher-idle/backstop retry cycles while the TUI stays busy:
    // promote (soft-take) → dispatch → gate defers. Every cycle must be
    // invisible to the user (reaction ops, view bytes, message posts: 0).
    for cycle in 0..3 {
        let taken = shared
            .mailbox(channel_id)
            .take_next_soft(persistence.clone())
            .await;
        let intervention = taken
            .intervention
            .unwrap_or_else(|| panic!("cycle {cycle}: the head must still be promotable"));
        let result = gateway
            .dispatch_queued_turn(
                channel_id,
                &intervention,
                "tester",
                false,
                taken.dispatch_lease,
            )
            .await;
        assert!(
            result.is_ok(),
            "cycle {cycle}: busy defer must report Ok so the epilogue does not requeue-front + fast-kick again"
        );

        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(
            snapshot.intervention_queue.len(),
            1,
            "cycle {cycle}: the deferred head is re-preserved in the queue (no duplicates)"
        );
        assert_eq!(
            snapshot.intervention_queue[0].message_id, user_msg,
            "cycle {cycle}: the SAME intervention (merged identity intact) sits back at the front"
        );
        assert_eq!(
            snapshot.pending_user_dispatch, None,
            "cycle {cycle}: the front-requeue consumes the stale dispatch reservation"
        );
        assert!(
            snapshot.cancel_token.is_none(),
            "cycle {cycle}: the mailbox is never claimed by the deferred promotion"
        );
        assert_eq!(
            shared.turn_view_reconciler.ops().len(),
            ops_before,
            "cycle {cycle}: the busy defer performs ZERO reaction ops — no ⏳ flip, no 📬 drain"
        );
        let view_after = std::fs::read(&view_path).expect("read turn view state after defer");
        assert_eq!(
            view_before, view_after,
            "cycle {cycle}: the head's persisted 📬 Queued turn-view state survives byte-for-byte"
        );
        assert_eq!(
            shared
                .restart
                .deferred_hook_backlog
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "cycle {cycle}: the slow (60s) fail-open backstop stays coalesced to one — no fast kick"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_promote_gate_capped_busy_retry_skips_backstop_4888() {
    let _root = scoped_runtime_root();
    let _busy = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(true);

    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    shared
        .turn_view_reconciler
        .record_test_reactions(http.clone());
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(100_000_004_888_600);
    let user_msg = MessageId::new(100_000_004_888_601);
    crate::services::discord::busy_followup_retry_store::bind_notice_if_absent(
        &provider,
        channel_id.get(),
        user_msg.get(),
        user_msg.get() + 1,
    )
    .expect("bind busy notice");
    for _ in 0..crate::services::discord::busy_followup_retry_store::MAX_BUSY_RETRY_COUNT {
        crate::services::discord::busy_followup_retry_store::record_busy_retry(
            &provider,
            channel_id.get(),
            user_msg.get(),
            user_msg.get() + 1,
        )
        .expect("record busy retry");
    }
    assert!(
        crate::services::discord::busy_followup_retry_store::is_capped(
            &provider,
            channel_id.get(),
            user_msg.get(),
        )
    );

    let persistence =
        crate::services::discord::queue_persistence_context(&shared, &provider, channel_id);
    shared
        .mailbox(channel_id)
        .replace_queue(
            vec![promoted_intervention(
                user_msg.get(),
                "capped busy promote head",
            )],
            persistence.clone(),
        )
        .await;
    let taken = shared.mailbox(channel_id).take_next_soft(persistence).await;
    let intervention = taken.intervention.expect("soft-take promotes the head");
    let gateway = DiscordGateway::new(http, shared.clone(), provider, None);

    assert!(
        gateway
            .dispatch_queued_turn(
                channel_id,
                &intervention,
                "tester",
                false,
                taken.dispatch_lease,
            )
            .await
            .is_ok()
    );
    let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(snapshot.intervention_queue[0].message_id, user_msg);
    assert_eq!(
        shared
            .restart
            .deferred_hook_backlog
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a capped busy retry must preserve the queue entry without arming the slow backstop"
    );
    assert!(
        !shared
            .restart
            .deferred_hook_channels
            .contains_key(&channel_id),
        "a capped busy retry must not register a deferred backstop task"
    );
}

/// #4270 pin (entrypoint, ready) — with the hosted TUI ready the promote
/// gate returns `false` and `dispatch_queued_turn` proceeds INTO the
/// dispatch body (proven by reaching the live-context requirement past the
/// gate). The gate itself touches neither the queue nor the reservation nor
/// the turn view on the ready path.
#[tokio::test(flavor = "current_thread")]
async fn dispatch_promote_gate_ready_passes_through_to_dispatch_4270() {
    let _root = scoped_runtime_root();
    let _ready = crate::services::discord::router::set_hosted_tui_promote_busy_for_tests(false);

    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    shared
        .turn_view_reconciler
        .record_test_reactions(http.clone());
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(100_000_004_270_700);
    let user_msg = MessageId::new(100_000_004_270_701);

    let persistence =
        crate::services::discord::queue_persistence_context(&shared, &provider, channel_id);
    shared
        .mailbox(channel_id)
        .replace_queue(
            vec![promoted_intervention(user_msg.get(), "ready promote head")],
            persistence.clone(),
        )
        .await;
    let taken = shared.mailbox(channel_id).take_next_soft(persistence).await;
    let intervention = taken.intervention.expect("soft-take promotes the head");
    let ops_before = shared.turn_view_reconciler.ops().len();

    let gateway = DiscordGateway::new(http.clone(), shared.clone(), provider.clone(), None);
    let result = gateway
        .dispatch_queued_turn(
            channel_id,
            &intervention,
            "tester",
            false,
            taken.dispatch_lease,
        )
        .await;
    let error = result.expect_err(
        "gate pass-through must continue into the dispatch body, which requires live context",
    );
    assert!(
        error.contains("missing live Discord context"),
        "the ready path reaches the dispatch body past the gate (got: {error})"
    );

    let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
    assert!(
        snapshot.intervention_queue.is_empty(),
        "the ready pass-through leaves the soft-taken head with the caller (no gate requeue)"
    );
    assert_eq!(
        snapshot.pending_user_dispatch,
        Some(user_msg),
        "the dispatch reservation stays with the in-flight promotion on the ready path"
    );
    assert_eq!(
        shared.turn_view_reconciler.ops().len(),
        ops_before,
        "the gate itself performs no reaction ops on the ready path"
    );
    assert_eq!(
        shared
            .restart
            .deferred_hook_backlog
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "no backstop is armed by a ready pass-through"
    );
}
