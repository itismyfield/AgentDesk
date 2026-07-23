//! #3894 — finalize side-effect chokepoint split out of `turn_finalizer.rs`.
//!
//! PURE MOVE (no logic change): `do_finalize`, the single owner of finalize's
//! side-effects (inflight clear / mailbox token release / `global_active`
//! decrement / trailing channel cleanup / relay-miss
//! observability, plus the #3866 in-finalize panic-injection test hook), lifted
//! verbatim. Re-imported by the parent (`use self::finalize::do_finalize`) so
//! `handle_terminal` and the reconcile/backstop child call it byte-identically.
//! Bare `super::` references to the discord module are rewritten to the absolute
//! `crate::services::discord::` path (house style, cf. `watcher_backstop.rs`) and
//! the sibling `cleanup` / `test_panic_hook` modules to `super::*` since the
//! module depth changed; the bodies are otherwise identical.

use super::*;

/// The single owner of finalize's side-effects. Reproduces today's exact
/// `clear_inflight (per-site) + mailbox_finish_turn + counter-- + trailing
/// terminal side-effects` sequence so each routed call-site stays
/// behaviourally identical during the incremental landing.
pub(super) async fn do_finalize(
    key: TurnKey,
    provider: ProviderKind,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    submit_snapshot: Option<&super::cleanup::SyntheticClaimSnapshot>,
    shared: &Arc<SharedData>,
) -> FinalizeOutcome {
    let channel_id = key.channel_id;

    // #3866: test-only injection point — fire a panic INSIDE the finalize
    // side-effect surface, AFTER the caller (`handle_terminal` /
    // `run_backstop_finalize`) flipped the entry to `Finalizing`, to prove the
    // caught-panic path still resets the entry to `Finalized` (never stuck
    // Finalizing) on BOTH the terminal and reconcile/backstop paths. No-op in
    // production builds.
    #[cfg(test)]
    super::test_panic_hook::maybe_panic_in_finalize();

    // #3866 residual (KNOWN, intentionally NOT fixed in this panic-guard pass —
    // tracked for a follow-up): (1) a poisoned mutex anywhere in the finalize
    // tree still propagates as a panic; it is now CONTAINED by the catch_unwind
    // guards (the loop survives, the entry is reset), but the underlying lock
    // stays poisoned. (2) The token-removal -> `saturating_decrement_global_active`
    // window below (B)->(C) is not atomic: a panic BETWEEN them caught by the
    // guard leaks one `global_active` count (the saturating decrement bounds the
    // blast radius to a single unit; it never underflows). (3) An id-0 channel-only
    // terminal that finalizes here gets the unguarded channel-scoped finish and no
    // per-submitter compensation, so a caught panic on that path cannot
    // selectively undo a partial finish. None of these block the panic-guard fix.
    //
    // #3350 ②: ensure the #3303 DeferredClaim marker for a watcher-owned TUI-direct
    // synthetic turn BEFORE (A) erases the row evidence. Codex r1-1: watcher
    // submitters cleared the row pre-submit, so for them this row re-load proves
    // nothing — their guarantee runs at submit time from the pre-clear snapshot
    // (`submit_terminal_with_claim_snapshot`); rationale/gates: cleanup.rs.
    super::cleanup::ensure_synthetic_claim_marker_before_clear(key, &provider, submit_snapshot);
    super::cleanup::enqueue_terminal_status_panel_reconcile(
        key,
        &provider,
        event,
        submit_snapshot,
        shared.as_ref(),
    );
    let skip_completion_reaction =
        super::cleanup::relay_ownership_only_for_finalize(key, &provider, submit_snapshot);
    let relay_owner_kind =
        super::cleanup::relay_owner_kind_for_finalize(key, &provider, submit_snapshot);

    // (A) inflight clear. Only the gate-timeout backstop and the immediate
    //     no-owner restored-watcher path set `clear_inflight` (live bridge /
    //     watcher sites clear inline, pass `false`). They consolidate the
    //     pre-#3016 IDENTITY-GUARDED 1800s sweeper: a real identity clears via
    //     `clear_inflight_state_if_matches` — never a newer turn's inflight,
    //     preserving `PlannedRestartSkipped` / `RebindOriginSkipped`; a true
    //     orphan (id-0, nothing to authenticate) keeps the unguarded clear.
    if ctx.clear_inflight {
        if key.user_msg_id != 0 {
            let _ = crate::services::discord::inflight::clear_inflight_state_if_matches(
                &provider,
                channel_id.get(),
                key.user_msg_id,
            );
        } else {
            crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
        }
    }

    // (B) mailbox cancel_token release — the routed sites' single, idempotent
    //     `mailbox_finish_turn` (`removed_token = None` on a second call).
    //     #3016 root-cause: a real identity uses the IDENTITY-GUARDED finish so
    //     finalize only releases the token it owns — a stale channel-scoped
    //     terminal post-finalize/ledger-GC must not release the NEWER turn's
    //     token or decrement `global_active`. Ambiguous id-0 (recovery/orphan)
    //     keeps the channel-scoped finish (ledger gate + id-0 no-op bound it).
    let owned_role_override = super::cleanup::snapshot_role_override(shared, channel_id);
    let finish = if key.user_msg_id != 0 {
        crate::services::discord::mailbox_finish_turn_if_matches(
            shared,
            &provider,
            channel_id,
            serenity::model::id::MessageId::new(key.user_msg_id),
        )
        .await
    } else {
        crate::services::discord::mailbox_finish_turn(shared, &provider, channel_id).await
    };

    if let Some(token) = finish.removed_token.as_ref() {
        // A normal completion releases lingering token observers via
        // `mark_completion_cleanup` so provider watchdogs don't treat the
        // post-terminal `cancelled` flip as a live mid-stream cancel. A real
        // cancel must NOT mark completion-cleanup; nor does the watcher path
        // (it historically only set `cancelled`).
        if ctx.allow_completion_cleanup && !matches!(event, TerminalEvent::Cancel) {
            token.mark_completion_cleanup();
        }
        // Stop any lingering watchdog timer from firing on a newer turn's
        // token.
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // (C) #3019 active-counter — decrement ONLY here, ONLY when this submission
    //     actually removed the active turn. Gating on `removed_token.is_some()`
    //     is what guarantees no underflow even under a transitional
    //     double-call.
    if finish.removed_token.is_some() {
        let decremented = crate::services::discord::saturating_decrement_global_active(shared);
        let global_active = shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::Relaxed);
        tracing::debug!(
            target: "agentdesk::global_active",
            channel_id = channel_id.get(),
            user_msg_id = key.user_msg_id,
            global_active,
            decremented,
            "global_active decrement"
        );
    }
    // The CHANNEL-SCOPED trailing side-effects (D)/(E) below mutate per-channel
    // routing/watchdog state that belongs to whatever turn is CURRENTLY active
    // in the channel. They are safe to run when this finalize actually finished
    // the turn (`removed_token.is_some()`), and harmlessly idempotent on the
    // legacy unguarded id-0 path (which always ran them). But when the
    // IDENTITY-GUARDED finish MISSED — a real `user_msg_id` that did NOT match
    // the live active turn, so `removed_token` is `None` — a DIFFERENT (newer)
    // turn owns the channel. Running these would clear the newer turn's
    // watchdog override, drop its `dispatch_thread_parents` / `dispatch_role_
    // overrides`, and drain its voice deferrals, corrupting a turn this stale
    // terminal does not own (Codex P2). So when the guard was used and missed,
    // skip the channel cleanup entirely — exactly as we already skip the token
    // release and counter decrement. (An id-0 orphan keeps today's behaviour.)
    let guarded_finish_missed = key.user_msg_id != 0 && finish.removed_token.is_none();
    if guarded_finish_missed {
        let active_user_message_id = crate::services::discord::mailbox_snapshot(shared, channel_id)
            .await
            .active_user_message_id
            .map(|id| id.get());
        if ctx.is_backstop_reconcile_path() {
            tracing::debug!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                expected_user_msg_id = key.user_msg_id,
                active_user_message_id = active_user_message_id.unwrap_or(0),
                generation = key.generation,
                expected_idempotent = true,
                "TurnFinalizer identity-guarded mailbox release skipped; active mailbox owner did not match finalizer turn identity"
            );
        } else {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                expected_user_msg_id = key.user_msg_id,
                active_user_message_id = active_user_message_id.unwrap_or(0),
                generation = key.generation,
                expected_idempotent = false,
                "TurnFinalizer identity-guarded mailbox release skipped; active mailbox owner did not match finalizer turn identity"
            );
        }
    }

    super::cleanup::finalized_reaction_lifecycle(
        key,
        event,
        ctx,
        shared,
        "finalized",
        skip_completion_reaction,
        relay_owner_kind,
    );

    let has_pending_after_voice = if guarded_finish_missed {
        // No-op finalize on a stale terminal: leave the live newer turn's
        // channel state untouched. Report NO backlog (Codex P2): the newer turn
        // is still active and owns its queue. Surfacing `finish.has_pending`
        // here would let the bridge propagate `has_queued_turns` and later drain
        // a queued soft message behind the live turn — concurrently dispatching
        // a follow-up this stale terminal does not own. A guarded miss is a true
        // no-op: no completion-event queue drain, no backlog reporting.
        false
    } else {
        // (D) trailing terminal side-effects that today follow
        //     `mailbox_finish_turn` inline at the bridge/watcher call-sites.
        //     Moved here so they cannot diverge between the routed paths.
        super::cleanup::clear_watchdog_and_kick_thread_parents_after_turn_release(
            shared, &provider, channel_id,
        )
        .await;

        let voice_deferred_enqueued = if ctx.drain_voice {
            shared
                .voice_barge_in
                .drain_deferred_after_turn(shared, &provider, channel_id)
                .await
        } else {
            false
        };
        let has_pending_after_voice = finish.has_pending || voice_deferred_enqueued;
        if !has_pending_after_voice {
            super::cleanup::remove_owned_role_override(shared, channel_id, owned_role_override);
        }

        // (E) Queue kickoff is owned by the #4048 completion-event listener. The
        // mailbox release primitive publishes the channel event as soon as it
        // removes the active token, and the listener kicks from a fresh mailbox
        // snapshot. That keeps completion rendering and queue drain timing
        // decoupled while preserving the real-turn safety gates.
        has_pending_after_voice
    };

    // (F) relay-miss observability — emitted from inside the finalizer so the
    //     signal fires exactly once per finalize regardless of submitter.
    if matches!(event, TerminalEvent::RelayMiss) {
        crate::services::observability::emit_inflight_lifecycle_event(
            provider.as_str(),
            channel_id.get(),
            None,
            None,
            None,
            "relay_miss_finalized",
            serde_json::json!({
                "removed_token": finish.removed_token.is_some(),
                "has_pending": has_pending_after_voice,
            }),
        );
    }

    FinalizeOutcome::Finalized {
        removed_token: finish.removed_token,
        has_pending: has_pending_after_voice,
        mailbox_online: finish.mailbox_online,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serenity::model::id::{MessageId, UserId};
    use std::io::{self, Write};
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct CapturingWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    struct EnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_root(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.as_ref() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn identity_guard_mismatch_does_not_release_wrong_owner_and_logs() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .expect("shared env lock poisoned");
        let root = tempfile::tempdir().expect("runtime root");
        let _env = EnvGuard::set_root(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_018_101);
        let stale_user_msg_id = 4_018_111;
        let active_user_msg_id = 4_018_222;
        let active_token = Arc::new(CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel_id,
                active_token.clone(),
                UserId::new(7),
                MessageId::new(active_user_msg_id),
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(CapturingWriter {
                buffer: buffer.clone(),
            })
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let outcome = do_finalize(
            TurnKey::new(channel_id, stale_user_msg_id, 0),
            ProviderKind::Claude,
            &TerminalEvent::Complete,
            FinalizeContext::bridge(),
            None,
            &shared,
        )
        .await;
        drop(_guard);

        match outcome {
            FinalizeOutcome::Finalized {
                removed_token,
                has_pending,
                ..
            } => {
                assert!(removed_token.is_none());
                assert!(!has_pending);
            }
            _ => panic!("direct do_finalize should return Finalized on a guarded miss"),
        }
        assert!(!active_token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        assert_eq!(
            crate::services::discord::mailbox_snapshot(&shared, channel_id)
                .await
                .active_user_message_id,
            Some(MessageId::new(active_user_msg_id))
        );
        let logs = String::from_utf8_lossy(&buffer.lock().unwrap()).into_owned();
        assert!(
            logs.contains("TurnFinalizer identity-guarded mailbox release skipped"),
            "identity mismatch must be operator-visible; logs={logs}"
        );
        assert!(logs.contains("expected_user_msg_id=4018111"), "{logs}");
        assert!(logs.contains("active_user_message_id=4018222"), "{logs}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn finalize_chokepoint_publishes_mailbox_release_completion_event() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .expect("shared env lock poisoned");
        let root = tempfile::tempdir().expect("runtime root");
        let _env = EnvGuard::set_root(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_048_100);
        let user_msg_id = 4_048_101;
        let token = Arc::new(CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel_id,
                token,
                UserId::new(9),
                MessageId::new(user_msg_id),
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);
        let mut rx =
            crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
                shared.as_ref(),
            );

        let outcome = do_finalize(
            TurnKey::new(channel_id, user_msg_id, 0),
            ProviderKind::Claude,
            &TerminalEvent::Complete,
            FinalizeContext::bridge(),
            None,
            &shared,
        )
        .await;

        assert!(matches!(
            outcome,
            FinalizeOutcome::Finalized {
                removed_token: Some(_),
                ..
            }
        ));
        assert!(
            crate::services::discord::mailbox_snapshot(&shared, channel_id)
                .await
                .cancel_token
                .is_none(),
            "completion event is published from the finalize chokepoint after the mailbox token is released"
        );
        let event = rx
            .try_recv()
            .expect("completion event should publish synchronously");
        assert_eq!(event.channel_id, channel_id);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn thread_finalize_removes_parent_mapping_and_schedules_parent_kickoff() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .expect("shared env lock poisoned");
        let root = tempfile::tempdir().expect("runtime root");
        let _env = EnvGuard::set_root(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let parent = ChannelId::new(4_024_190);
        let thread = ChannelId::new(4_024_191);
        let user_msg_id = 4_024_192;
        shared.dispatch.thread_parents.insert(parent, thread);
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                thread,
                Arc::new(CancelToken::new()),
                UserId::new(9),
                MessageId::new(user_msg_id),
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let outcome = do_finalize(
            TurnKey::new(thread, user_msg_id, 0),
            ProviderKind::Claude,
            &TerminalEvent::Complete,
            FinalizeContext::bridge(),
            None,
            &shared,
        )
        .await;

        assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
        assert!(
            !shared.dispatch.thread_parents.contains_key(&parent),
            "finalizing the thread turn must drop its parent mapping"
        );
        assert_eq!(
            shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
            1,
            "dropping the parent mapping must schedule the parent queue kick"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn guarded_miss_preserves_parent_mapping_and_skips_parent_kickoff() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .expect("shared env lock poisoned");
        let root = tempfile::tempdir().expect("runtime root");
        let _env = EnvGuard::set_root(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let parent = ChannelId::new(4_024_193);
        let thread = ChannelId::new(4_024_194);
        let stale_user_msg_id = 4_024_195;
        let active_user_msg_id = 4_024_196;
        shared.dispatch.thread_parents.insert(parent, thread);
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                thread,
                Arc::new(CancelToken::new()),
                UserId::new(9),
                MessageId::new(active_user_msg_id),
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let outcome = do_finalize(
            TurnKey::new(thread, stale_user_msg_id, 0),
            ProviderKind::Claude,
            &TerminalEvent::Complete,
            FinalizeContext::bridge(),
            None,
            &shared,
        )
        .await;

        assert!(matches!(
            outcome,
            FinalizeOutcome::Finalized {
                removed_token: None,
                ..
            }
        ));
        assert!(
            shared
                .dispatch
                .thread_parents
                .get(&parent)
                .is_some_and(|thread_id| *thread_id == thread),
            "guarded-miss finalize must leave thread-parent mappings untouched"
        );
        assert_eq!(
            shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
            0,
            "guarded-miss finalize must not schedule a parent kick"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_thread_finalize_schedules_no_parent_kickoff() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .expect("shared env lock poisoned");
        let root = tempfile::tempdir().expect("runtime root");
        let _env = EnvGuard::set_root(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(4_024_197);
        let user_msg_id = 4_024_198;
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel_id,
                Arc::new(CancelToken::new()),
                UserId::new(9),
                MessageId::new(user_msg_id),
            )
            .await
        );
        shared.restart.global_active.store(1, Ordering::Relaxed);

        let outcome = do_finalize(
            TurnKey::new(channel_id, user_msg_id, 0),
            ProviderKind::Claude,
            &TerminalEvent::Complete,
            FinalizeContext::bridge(),
            None,
            &shared,
        )
        .await;

        assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
        assert_eq!(
            shared.restart.deferred_hook_backlog.load(Ordering::Relaxed),
            0,
            "no removed thread-parent mapping means no parent kick"
        );
    }
}
