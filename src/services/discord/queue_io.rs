use super::*;

#[cfg_attr(not(test), allow(dead_code))]
fn prune_interventions_at(queue: &mut Vec<Intervention>, now: Instant) {
    queue.retain(|i| now.duration_since(i.created_at) <= INTERVENTION_TTL);
    if queue.len() > MAX_INTERVENTIONS_PER_CHANNEL {
        let overflow = queue.len() - MAX_INTERVENTIONS_PER_CHANNEL;
        queue.drain(0..overflow);
    }
}

#[allow(dead_code)]
pub(super) fn channel_has_pending_soft_queue(
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
) -> bool {
    channel_has_pending_soft_queue_at(intervention_queue, channel_id, Instant::now())
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn channel_has_pending_soft_queue_at(
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
    now: Instant,
) -> bool {
    let mut remove_queue = false;
    let has_pending = if let Some(queue) = intervention_queue.get_mut(&channel_id) {
        prune_interventions_at(queue, now);
        let has_pending = queue.iter().any(|item| item.mode == InterventionMode::Soft);
        remove_queue = queue.is_empty();
        has_pending
    } else {
        false
    };
    if remove_queue {
        intervention_queue.remove(&channel_id);
    }
    has_pending
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn watcher_should_kickoff_idle_queue(
    has_active_turn: bool,
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
) -> bool {
    watcher_should_kickoff_idle_queue_at(
        has_active_turn,
        intervention_queue,
        channel_id,
        Instant::now(),
    )
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn watcher_should_kickoff_idle_queue_at(
    has_active_turn: bool,
    intervention_queue: &mut HashMap<ChannelId, Vec<Intervention>>,
    channel_id: ChannelId,
    now: Instant,
) -> bool {
    if has_active_turn {
        return false;
    }
    channel_has_pending_soft_queue_at(intervention_queue, channel_id, now)
}

/// #2044 F3: RAII guard that ensures `deferred_hook_backlog` is
/// decremented even if the spawned future panics inside
/// `kickoff_idle_queues` (which awaits multiple IO calls and may
/// unwind). The previous code used a plain `fetch_sub` at the end of
/// the spawned future, so any panic between the matching `fetch_add`
/// and the trailing decrement permanently leaked the counter — which
/// the shutdown drain loop and operator dashboards both rely on for
/// "is the deferred backlog empty yet?" decisions.
struct DeferredHookBacklogGuard {
    shared: Arc<SharedData>,
}

const DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY: std::time::Duration =
    std::time::Duration::from_secs(2);
const DEFERRED_IDLE_QUEUE_KICKOFF_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_secs(2);
// Keep retrying long enough to cover dcserver/gateway restart windows. A
// queued user reply should not wait for the next external Discord event just
// because cached ctx/token arrived slightly after the first post-turn kickoff.
const DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS: usize = 150;

#[cfg_attr(not(test), allow(dead_code))]
fn should_retry_deferred_idle_queue_kickoff(attempt: usize) -> bool {
    attempt < DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
}

/// #3005: pre-sleep before the very first deferred-drain attempt. The
/// finalize-completed (idle-confirmed) path passes `immediate_once = true` so
/// the first kickoff runs without the 2s `INITIAL_DELAY` guard; every other
/// caller keeps the full delay to avoid spinning during the dcserver/gateway
/// restart window.
#[cfg_attr(not(test), allow(dead_code))]
fn deferred_idle_queue_initial_presleep(immediate_once: bool) -> std::time::Duration {
    if immediate_once {
        std::time::Duration::ZERO
    } else {
        DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY
    }
}

impl Drop for DeferredHookBacklogGuard {
    fn drop(&mut self) {
        self.shared
            .deferred_hook_backlog
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(super) fn schedule_deferred_idle_queue_kickoff(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) {
    schedule_deferred_idle_queue_kickoff_inner(shared, provider, channel_id, reason, false);
}

/// #3005: variant for the finalize-completed (idle-confirmed) path. When a turn
/// has just finalized with a confirmed-idle pane and a queued backlog remains,
/// the first kickoff attempt skips the 2s `INITIAL_DELAY` pre-sleep and tries
/// `kickoff_idle_queues` immediately, falling back to the existing 2s retry
/// cadence if that first attempt cannot drain (e.g. cached ctx/token not yet
/// available, or the hosted TUI is still transiently `Busy`). The
/// `INITIAL_DELAY` constant is intentionally left untouched — it still guards
/// the restart-window spin for every other caller — so this only narrows the
/// post-finalize latency where idle has already been confirmed.
pub(super) fn schedule_deferred_idle_queue_kickoff_immediate(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
) {
    schedule_deferred_idle_queue_kickoff_inner(shared, provider, channel_id, reason, true);
}

fn schedule_deferred_idle_queue_kickoff_inner(
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    reason: &'static str,
    immediate_once: bool,
) {
    shared
        .deferred_hook_backlog
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    super::task_supervisor::spawn_observed("deferred_idle_queue_kickoff", async move {
        // #2044 F3: bind the decrement to a Drop guard so it fires on
        // panic-unwind as well as on normal return.
        let _backlog_guard = DeferredHookBacklogGuard {
            shared: shared.clone(),
        };
        // #3005: on the finalize-completed (idle-confirmed) reason the first
        // attempt skips the 2s pre-sleep so a queued follow-up can drain right
        // after the turn settles; all subsequent attempts keep the 2s cadence.
        let initial_presleep = deferred_idle_queue_initial_presleep(immediate_once);
        if !initial_presleep.is_zero() {
            tokio::time::sleep(initial_presleep).await;
        }
        for attempt in 1..=DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS {
            if let (Some(ctx), Some(tok)) = (
                shared.cached_serenity_ctx.get(),
                shared.cached_bot_token.get(),
            ) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🚀 Deferred drain: kicking off idle queues for channel {} ({reason}, attempt {attempt}/{})",
                    channel_id,
                    DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
                );
                let started = super::kickoff_idle_queues(ctx, &shared, tok, &provider).await;
                // Always re-check the queue on a zero-start drain regardless
                // of `reason`. The earlier `should_retry_zero_start_*` reason
                // allowlist gated retry on a single hard-coded reason
                // (`hosted_tui_busy_pre_submit_pending`) so every other
                // caller (monitor turn completion, placeholder_sweeper,
                // catch-up retry, tmux stop, …) silently abandoned the
                // queue when kickoff returned 0 — usually because the
                // hosted TUI was still `Busy` for the few seconds after a
                // turn finished. The blocker is the TUI ready state, not
                // the reason string, so any reason can hit the same
                // transient window. Retry whenever the queue is still
                // non-empty within the bounded attempt budget; the
                // hosted-TUI gate inside `kickoff_idle_queues` is what
                // keeps us from racing a still-busy pane.
                let target_still_pending = !super::mailbox_snapshot(shared.as_ref(), channel_id)
                    .await
                    .intervention_queue
                    .is_empty();
                if started == 0
                    && target_still_pending
                    && should_retry_deferred_idle_queue_kickoff(attempt)
                {
                    tracing::info!(
                        "  [{ts}] ⏳ Deferred drain: channel {} still queued after zero-start drain ({reason}, attempt {attempt}/{}); retrying",
                        channel_id,
                        DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
                    );
                    tokio::time::sleep(DEFERRED_IDLE_QUEUE_KICKOFF_RETRY_DELAY).await;
                    continue;
                }
                return;
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            if !should_retry_deferred_idle_queue_kickoff(attempt) {
                tracing::warn!(
                    "  [{ts}] ⚠ Deferred drain: missing cached context for channel {} after {} attempts ({reason}); queued items remain persisted for next kickoff",
                    channel_id,
                    DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
                );
                break;
            }
            tracing::info!(
                "  [{ts}] ⚠ Deferred drain: missing cached context for channel {} ({reason}, attempt {attempt}/{}); retrying",
                channel_id,
                DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
            );
            tokio::time::sleep(DEFERRED_IDLE_QUEUE_KICKOFF_RETRY_DELAY).await;
        }
        // Drop guard at end of scope decrements the backlog counter.
    });
}

#[cfg(test)]
mod presleep_tests {
    use super::*;

    /// #3005: the finalize-completed immediate path must skip the 2s
    /// INITIAL_DELAY pre-sleep on the first attempt, while every other caller
    /// keeps the full delay (restart-window spin guard). The INITIAL_DELAY
    /// constant itself must remain 2s — only the immediate flag bypasses it.
    #[test]
    fn immediate_once_skips_initial_presleep() {
        assert_eq!(
            deferred_idle_queue_initial_presleep(true),
            std::time::Duration::ZERO,
            "finalize-completed immediate path must not pre-sleep"
        );
        assert_eq!(
            deferred_idle_queue_initial_presleep(false),
            DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY,
            "non-immediate callers keep the full INITIAL_DELAY"
        );
        // Guard against silently lowering the shared constant (issue rule).
        assert_eq!(
            DEFERRED_IDLE_QUEUE_KICKOFF_INITIAL_DELAY,
            std::time::Duration::from_secs(2)
        );
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::services::discord::runtime_store::test_env_lock;
    use crate::services::turn_orchestrator::{
        PendingQueueItem, load_pending_queues, requeue_intervention_front_persisted,
        save_channel_queue, save_pending_queues, take_next_soft_intervention_persisted,
    };

    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    const AGENTDESK_ROOT_DIR_ENV: &str = "AGENTDESK_ROOT_DIR";

    fn make_intervention(text: &str) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(100),
            source_message_ids: vec![MessageId::new(100)],
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    fn lock_test_env() -> std::sync::MutexGuard<'static, ()> {
        test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Regression guard: zero-start retry must NOT depend on a `reason`
    /// allowlist. Earlier code limited retry to the single
    /// `hosted_tui_busy_pre_submit_pending` reason, so every other caller
    /// (monitor turn completion, placeholder_sweeper, catch-up retry, tmux
    /// stop, …) silently abandoned the queue when the hosted TUI was
    /// transiently `Busy` at drain time — the recurring "no active turn
    /// but queue stays full" symptom. The retry decision is now driven by
    /// the live queue snapshot and the bounded attempt budget; any future
    /// regression that re-introduces reason-based gating should fail
    /// here.
    #[test]
    fn zero_start_deferred_retry_is_not_gated_by_reason() {
        // The reason-allowlist helper has been deleted on purpose. If a
        // future change re-introduces a per-reason zero-start gate the
        // commit must also re-explain why the cross-caller orphan-queue
        // failure mode is acceptable; this test will need to be updated
        // alongside.
        assert!(should_retry_deferred_idle_queue_kickoff(1));
        assert!(!should_retry_deferred_idle_queue_kickoff(
            DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
        ));
    }

    #[test]
    fn channel_has_pending_soft_queue_detects_live_backlog() {
        let channel_id = ChannelId::new(12345);
        let created_at = Instant::now();
        let mut queues = HashMap::new();
        queues.insert(
            channel_id,
            vec![Intervention {
                author_id: UserId::new(42),
                author_is_bot: false,
                message_id: MessageId::new(7),
                source_message_ids: vec![MessageId::new(7)],
                text: "pending".to_string(),
                mode: InterventionMode::Soft,
                created_at,
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            }],
        );

        assert!(channel_has_pending_soft_queue_at(
            &mut queues,
            channel_id,
            created_at
        ));
        assert!(queues.contains_key(&channel_id));
    }

    #[test]
    fn channel_has_pending_soft_queue_prunes_expired_entries() {
        let channel_id = ChannelId::new(12345);
        let created_at = Instant::now();
        let mut queues = HashMap::new();
        queues.insert(
            channel_id,
            vec![Intervention {
                author_id: UserId::new(42),
                author_is_bot: false,
                message_id: MessageId::new(7),
                source_message_ids: vec![MessageId::new(7)],
                text: "stale".to_string(),
                mode: InterventionMode::Soft,
                created_at,
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            }],
        );

        assert!(!channel_has_pending_soft_queue_at(
            &mut queues,
            channel_id,
            created_at + INTERVENTION_TTL + Duration::from_secs(1)
        ));
        assert!(!queues.contains_key(&channel_id));
    }

    #[test]
    fn watcher_should_kickoff_idle_queue_requires_idle_channel() {
        let channel_id = ChannelId::new(12345);
        let mut queues = HashMap::new();
        queues.insert(
            channel_id,
            vec![Intervention {
                author_id: UserId::new(42),
                author_is_bot: false,
                message_id: MessageId::new(7),
                source_message_ids: vec![MessageId::new(7)],
                text: "pending".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: Vec::new(),
                voice_announcement: None,
            }],
        );

        assert!(watcher_should_kickoff_idle_queue(
            false,
            &mut queues,
            channel_id
        ));
        assert!(!watcher_should_kickoff_idle_queue(
            true,
            &mut queues,
            channel_id
        ));
    }

    #[test]
    fn deferred_idle_queue_kickoff_retries_until_final_attempt() {
        assert!(should_retry_deferred_idle_queue_kickoff(1));
        assert!(should_retry_deferred_idle_queue_kickoff(
            DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS - 1
        ));
        assert!(!should_retry_deferred_idle_queue_kickoff(
            DEFERRED_IDLE_QUEUE_KICKOFF_MAX_ATTEMPTS
        ));
    }

    /// Queue files must land under `{provider}/{token_hash}/` — not the legacy flat path.
    #[test]
    fn pending_queue_path_uses_token_hash() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "abc123";
        let channel_id = ChannelId::new(999);

        let queue = vec![make_intervention("hello")];
        save_channel_queue(&provider, token_hash, channel_id, &queue, None).unwrap();

        let expected = tmp
            .path()
            .join("runtime")
            .join("discord_pending_queue")
            .join("claude")
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()));
        assert!(
            expected.exists(),
            "queue file not found at expected path: {expected:?}"
        );

        let legacy = tmp
            .path()
            .join("runtime")
            .join("discord_pending_queue")
            .join("claude")
            .join(format!("{}.json", channel_id.get()));
        assert!(
            !legacy.exists(),
            "queue file must not be written to legacy flat path"
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    /// Bot A writes a queue; Bot B (different token_hash) must not see it on load.
    #[test]
    fn load_pending_queues_only_reads_own_namespace() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let channel_id = ChannelId::new(42);

        save_channel_queue(
            &provider,
            "hash_bot_a",
            channel_id,
            &[make_intervention("from A")],
            None,
        )
        .unwrap();

        let (result, _overrides) = load_pending_queues(&provider, "hash_bot_b");
        assert!(result.is_empty(), "bot B must not restore bot A's queue");

        let (result, _overrides) = load_pending_queues(&provider, "hash_bot_a");
        assert_eq!(result.len(), 1, "bot A must restore its own queue");
        assert_eq!(result[&channel_id][0].text, "from A");

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    /// save_pending_queues + load_pending_queues round-trip with token_hash namespacing.
    /// Loading is intentionally non-destructive: startup restore may still reject
    /// an item after parsing it, so disk must remain the fallback until the
    /// mailbox later persists a changed queue.
    #[test]
    fn save_pending_queues_roundtrip() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "roundtrip_hash";
        let ch1 = ChannelId::new(1);
        let ch2 = ChannelId::new(2);

        let mut queues = HashMap::new();
        queues.insert(ch1, vec![make_intervention("msg1")]);
        queues.insert(
            ch2,
            vec![make_intervention("msg2a"), make_intervention("msg2b")],
        );

        save_pending_queues(&provider, token_hash, &queues, &dashmap::DashMap::new()).unwrap();

        let (restored, _restored_overrides) = load_pending_queues(&provider, token_hash);
        assert_eq!(restored.get(&ch1).map(|v| v.len()), Some(1));
        assert_eq!(restored.get(&ch2).map(|v| v.len()), Some(2));

        let dir = tmp
            .path()
            .join("runtime")
            .join("discord_pending_queue")
            .join("claude")
            .join(token_hash);
        assert!(dir.join("1.json").exists());
        assert!(dir.join("2.json").exists());

        let (restored_again, _restored_overrides_again) =
            load_pending_queues(&provider, token_hash);
        assert_eq!(restored_again.get(&ch1).map(|v| v.len()), Some(1));
        assert_eq!(restored_again.get(&ch2).map(|v| v.len()), Some(2));

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    #[test]
    fn persisted_queue_helpers_keep_remaining_items_and_restore_requeued_item() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "preserve_roundtrip";
        let channel_id = ChannelId::new(41);
        let alt_channel = ChannelId::new(99);

        let mut queues = HashMap::new();
        queues.insert(
            channel_id,
            vec![make_intervention("first"), make_intervention("second")],
        );
        let overrides: dashmap::DashMap<ChannelId, ChannelId> = dashmap::DashMap::new();
        overrides.insert(channel_id, alt_channel);
        save_pending_queues(&provider, token_hash, &queues, &overrides).unwrap();

        let (popped, has_more) = take_next_soft_intervention_persisted(
            &provider,
            token_hash,
            channel_id,
            &mut queues,
            &overrides,
        )
        .expect("queue item should be popped");
        assert_eq!(popped.text, "first");
        assert!(has_more);
        assert_eq!(queues.get(&channel_id).map(|items| items.len()), Some(1));

        let file = tmp
            .path()
            .join("runtime")
            .join("discord_pending_queue")
            .join("claude")
            .join(token_hash)
            .join(format!("{}.json", channel_id.get()));
        let content = std::fs::read_to_string(&file).unwrap();
        let items: Vec<PendingQueueItem> = serde_json::from_str(&content).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "second");
        assert_eq!(items[0].override_channel_id, Some(alt_channel.get()));

        requeue_intervention_front_persisted(
            &provider,
            token_hash,
            channel_id,
            &mut queues,
            &overrides,
            popped,
        );

        let content = std::fs::read_to_string(&file).unwrap();
        let items: Vec<PendingQueueItem> = serde_json::from_str(&content).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "first");
        assert_eq!(items[1].text, "second");
        assert_eq!(items[0].override_channel_id, Some(alt_channel.get()));
        assert_eq!(items[1].override_channel_id, Some(alt_channel.get()));

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    /// PendingQueueItem serializes routing snapshot fields and deserializes with defaults for old format.
    #[test]
    fn pending_queue_item_serde_backward_compatible() {
        let old_json = r#"[{"author_id":1,"message_id":100,"text":"hello"}]"#;
        let items: Vec<PendingQueueItem> = serde_json::from_str(old_json).unwrap();
        assert_eq!(items[0].text, "hello");
        assert!(items[0].source_message_ids.is_empty());
        assert!(!items[0].author_is_bot);
        assert!(items[0].reply_context.is_none());
        assert!(!items[0].has_reply_boundary);
        assert!(!items[0].merge_consecutive);
        assert!(items[0].channel_id.is_none());
        assert!(items[0].channel_name.is_none());
        assert!(items[0].override_channel_id.is_none());

        let new_item = PendingQueueItem {
            author_id: 1,
            author_is_bot: false,
            message_id: 100,
            source_message_ids: vec![100, 101],
            text: "hello".to_string(),
            reply_context: Some("[Reply context]".to_string()),
            has_reply_boundary: true,
            merge_consecutive: true,
            pending_uploads: Vec::new(),
            channel_id: Some(42),
            channel_name: Some("test-channel".to_string()),
            override_channel_id: None,
            voice_announcement: None,
        };
        let json = serde_json::to_string(&new_item).unwrap();
        let parsed: PendingQueueItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source_message_ids, vec![100, 101]);
        assert_eq!(parsed.reply_context.as_deref(), Some("[Reply context]"));
        assert!(parsed.has_reply_boundary);
        assert!(parsed.merge_consecutive);
        assert_eq!(parsed.channel_id, Some(42));
        assert_eq!(parsed.channel_name.as_deref(), Some("test-channel"));
    }

    /// P2: Two bots with empty or duplicate `agent` labels but different token hashes
    /// must not collide — the namespace key is token_hash, not agent.
    #[test]
    fn agent_empty_or_duplicate_does_not_collide_namespace() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let ch = ChannelId::new(77);

        save_channel_queue(&provider, "hash_x", ch, &[make_intervention("bot-x")], None).unwrap();
        save_channel_queue(&provider, "hash_y", ch, &[make_intervention("bot-y")], None).unwrap();

        let (result_x, _) = load_pending_queues(&provider, "hash_x");
        let (result_y, _) = load_pending_queues(&provider, "hash_y");

        assert_eq!(result_x.get(&ch).map(|v| v[0].text.as_str()), Some("bot-x"));
        assert_eq!(result_y.get(&ch).map(|v| v[0].text.as_str()), Some("bot-y"));

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    /// P2: review/reused thread override_channel_id survives a save/load round-trip.
    /// This ensures dispatch_role_overrides are not lost on restart.
    #[test]
    fn review_thread_override_preserved_across_restart() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "review_hash";
        let thread_channel = ChannelId::new(500);
        let alt_channel = ChannelId::new(501);

        save_channel_queue(
            &provider,
            token_hash,
            thread_channel,
            &[make_intervention("review msg")],
            Some(alt_channel.get()),
        )
        .unwrap();

        let (queues, overrides) = load_pending_queues(&provider, token_hash);
        assert_eq!(queues.get(&thread_channel).map(|v| v.len()), Some(1));
        assert_eq!(
            overrides.get(&thread_channel).copied(),
            Some(alt_channel),
            "override_channel_id must be restored from queue snapshot"
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    /// P2: save_pending_queues captures dispatch_role_overrides into override_channel_id.
    #[test]
    fn save_pending_queues_captures_dispatch_role_overrides() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let token_hash = "override_test";
        let ch = ChannelId::new(300);
        let alt_ch = ChannelId::new(301);

        let mut queues = HashMap::new();
        queues.insert(ch, vec![make_intervention("queued msg")]);

        let overrides: dashmap::DashMap<ChannelId, ChannelId> = dashmap::DashMap::new();
        overrides.insert(ch, alt_ch);

        save_pending_queues(&provider, token_hash, &queues, &overrides).unwrap();

        let dir = tmp
            .path()
            .join("runtime")
            .join("discord_pending_queue")
            .join("claude")
            .join(token_hash);
        let file = dir.join(format!("{}.json", ch.get()));
        let content = std::fs::read_to_string(&file).unwrap();
        let items: Vec<PendingQueueItem> = serde_json::from_str(&content).unwrap();
        assert_eq!(items[0].override_channel_id, Some(alt_ch.get()));

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }

    /// P2: Legacy flat queue files (old path without token_hash) are NOT loaded
    /// by load_pending_queues, which only reads from the token_hash subdirectory.
    #[test]
    fn legacy_flat_queue_file_is_not_restored() {
        let _lock = lock_test_env();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var(AGENTDESK_ROOT_DIR_ENV, tmp.path().to_str().unwrap()) };

        let provider = ProviderKind::Claude;
        let ch = ChannelId::new(999);

        let legacy_dir = tmp
            .path()
            .join("runtime")
            .join("discord_pending_queue")
            .join("claude");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy_file = legacy_dir.join(format!("{}.json", ch.get()));
        let item = PendingQueueItem {
            author_id: 1,
            author_is_bot: false,
            message_id: 100,
            source_message_ids: vec![100],
            text: "legacy msg".to_string(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            channel_id: None,
            channel_name: None,
            override_channel_id: None,
            voice_announcement: None,
        };
        std::fs::write(&legacy_file, serde_json::to_string(&vec![item]).unwrap()).unwrap();

        let (result, _) = load_pending_queues(&provider, "any_hash");
        assert!(
            result.is_empty(),
            "legacy flat file must not be restored by load_pending_queues"
        );
        assert!(
            legacy_file.exists(),
            "load_pending_queues must not delete legacy files"
        );

        unsafe { std::env::remove_var(AGENTDESK_ROOT_DIR_ENV) };
    }
}
