//! #3805 P2 (PR-C): two-message status-panel WATCHER creation-order parity.
//!
//! The tmux watcher owns a SECOND, fully independent relay path from the bridge
//! sink (`turn_bridge`). PR-B put the sink on the two-message layout (the answer
//! stays first/highest, the live status panel is created as a NEW message BELOW
//! it, opening a per-turn `status_panel_generation` epoch). The operator repro
//! was on the WATCHER path, so this module gives the watcher the SAME creation
//! order and generation semantics under the same default-OFF
//! `placeholder.two_message_panel_enabled` flag.
//!
//! Unlike the sink — which is turn-scoped and mutates a pinned in-memory
//! inflight snapshot — the watcher is NOT turn-scoped and races overlapping
//! watchers, so it keeps its existing atomic `bind_status_panel` publish machinery
//! and only THREADS the two-message decisions through it. This sibling therefore
//! holds just the small PURE predicates the (EXTREME 7122-line, 700-capped)
//! watcher giant and its `single_message_footer.rs` (near the 700 cap) call into
//! thinly:
//!
//! 1. `watcher_two_message_panel_creation_gated_by_answer` — defer panel creation
//!    until the ANSWER placeholder exists, so the panel is created BELOW it
//!    (answer-first). This is the watcher analog of the sink gate's "the answer
//!    is a real message" precondition. OFF: always `true` (byte-identical).
//! 2. `watcher_two_message_bind_generation` — the epoch to stamp when the panel
//!    is freshly bound (seed + 1), parity with the sink create's `saturating_add(1)`.
//!    OFF: `None` (the bind leaves the generation untouched).
//! 3. `watcher_two_message_status_completion_superseded` — the completion guard,
//!    reusing the ONE shared staleness predicate from PR-B's sink sibling
//!    (`turn_bridge::two_message_status_edit_generation_is_stale`) so the sink and
//!    watcher supersede a stale status edit by identical epoch rules. Inert while
//!    every generation is `0` (OFF) or equal (PR-C has no mid-turn re-anchor yet).

use super::*;

/// #3805 P2 (PR-C): under the two-message flag the watcher must create the status
/// panel BELOW the answer, so it must not publish the panel until the answer
/// placeholder message exists. Returns whether the watcher's separate-panel
/// creation block may proceed THIS interval.
///
/// - OFF (`two_message_panel_enabled == false`): always `true` — the existing
///   creation block runs exactly as today (byte-identical). The legacy layout is
///   unchanged (the panel may be created before the answer placeholder exists).
/// - ON: gate on `placeholder_present`. When the answer placeholder does not yet
///   exist the block is skipped for this interval; the streaming loop creates the
///   answer message first, and the next interval creates the panel BELOW it
///   (answer-first). Deferring by at most one interval never drops the panel —
///   the placeholder becomes `Some` in the same interval the block was skipped.
pub(in crate::services::discord) fn watcher_two_message_panel_creation_gated_by_answer(
    two_message_panel_enabled: bool,
    placeholder_present: bool,
) -> bool {
    !two_message_panel_enabled || placeholder_present
}

/// #3805 P2 (PR-C): the `status_panel_generation` to stamp when the watcher
/// FRESHLY binds the two-message panel — one past the turn's seed epoch, opening
/// this turn's panel epoch. Parity with the sink create's
/// `inflight_state.status_panel_generation.saturating_add(1)`.
///
/// Returns `None` on the OFF path (the bind guard leaves the generation
/// untouched → byte-identical) and `Some(seed + 1)` when the flag is ON. The
/// caller mirrors the same value into its per-turn generation local ONLY on a
/// genuine fresh `Bound`; an `AlreadyBound` re-bind does not re-open the epoch.
pub(in crate::services::discord) fn watcher_two_message_bind_generation(
    two_message_panel_enabled: bool,
    seed_generation: u64,
) -> Option<u64> {
    two_message_panel_enabled.then(|| seed_generation.saturating_add(1))
}

/// #3805 P2 (PR-C): the watcher completion guard — is this turn's status-panel
/// completion edit superseded by a NEWER epoch for the SAME owned panel on disk?
///
/// Reuses the ONE shared staleness predicate from PR-B's sink sibling so the sink
/// and watcher share identical epoch rules (parity). Ownership-scoped exactly
/// like the sink: only the panel THIS turn actually owns on disk can supersede
/// it, and a synthetic-headless id owns nothing. Inert on the default-OFF path
/// (every generation is `0`) and at PR-C (no mid-turn re-anchor bumps the epoch,
/// so this turn's local always equals the on-disk epoch); the later
/// re-anchor/recovery stages (PR-D/E) bump the epoch mid-turn so a stale in-flight
/// completion for the OLD generation is skipped here.
pub(in crate::services::discord) fn watcher_two_message_status_completion_superseded(
    this_turn_status_panel_generation: u64,
    status_panel_msg_id: Option<serenity::MessageId>,
    on_disk: Option<&InflightTurnState>,
) -> bool {
    let Some(on_disk) = on_disk else {
        return false;
    };
    let panel_owned_on_disk = match status_panel_msg_id {
        Some(id) if !crate::services::discord::is_synthetic_headless_message_id_raw(id.get()) => {
            on_disk.status_message_id == Some(id.get())
        }
        _ => false,
    };
    crate::services::discord::turn_bridge::two_message_status_edit_generation_is_stale(
        this_turn_status_panel_generation,
        panel_owned_on_disk,
        on_disk.status_panel_generation,
    )
}

/// #3805 P2 (PR-C): the watcher status-panel completion tail — apply the
/// generation guard, otherwise complete the panel, then reconcile the durable
/// orphan record.
///
/// The `generation_superseded` skip is the watcher parity of the sink completion
/// guard: a NEWER panel epoch for the SAME owned panel means a live re-anchored
/// panel (PR-D) now owns the surface, so this stale edit is skipped and treated
/// as committed (the panel this turn no longer owns is not enqueued as an
/// orphan). Inert on the default-OFF path (the caller passes `false`).
///
/// The completion + orphan reconcile is a VERBATIM move of the tail that used to
/// live in `single_message_footer.rs` (zero logic change for the panel path; the
/// footer path never reached the reconcile because footer mode carries no
/// separate panel id). Extracted here so the P2 guard lands in the sibling and
/// the 700-capped footer file stays lean.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn complete_watcher_status_panel_v2_with_generation_guard(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    started_at_unix: i64,
    status_panel_msg_id: Option<serenity::MessageId>,
    last_status_panel_text: &mut String,
    completion_background: bool,
    status_panel_completion_user_msg_id: Option<u64>,
    turn_is_external_input_for_session: bool,
    generation_superseded: bool,
) {
    let committed = if generation_superseded {
        tracing::debug!(
            "  [tmux_watcher] skipping status-panel-v2 completion edit of msg {:?} in channel {}: a newer panel epoch now owns the panel",
            status_panel_msg_id,
            channel_id.get()
        );
        true
    } else {
        complete_watcher_status_panel_v2(
            http,
            shared,
            channel_id,
            status_panel_msg_id,
            provider,
            started_at_unix,
            last_status_panel_text,
            completion_background,
            status_panel_completion_user_msg_id,
        )
        .await
    };
    if !turn_is_external_input_for_session {
        return;
    }
    let Some(panel_msg_id) = status_panel_msg_id else {
        return;
    };
    if committed {
        crate::services::discord::status_panel_orphan_store::remove(
            provider,
            &shared.token_hash,
            channel_id.get(),
            panel_msg_id.get(),
        );
    } else {
        enqueue_watcher_status_panel_orphan(shared.as_ref(), provider, channel_id, panel_msg_id);
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: status panel completion failed for channel {} msg {}; queued durable orphan cleanup",
            channel_id.get(),
            panel_msg_id.get()
        );
    }
}

/// #3805 P2 (PR-D): re-anchor the watcher's two-message status panel BELOW the
/// new answer chunk after a mid-turn rollover created a fresh tail message.
///
/// Parity with the sink re-anchor (`turn_bridge::two_message_panel`), but the
/// watcher is NOT turn-scoped, so the msg-id repoint + epoch bump go through the
/// atomic `bind_status_panel` flock (the same CAS store the create bind uses)
/// rather than an in-memory snapshot:
/// 1. Send the NEW panel BELOW the new tail answer (never a zero-panel window).
/// 2. `bind_status_panel(new_id, skip_if_panel_already_set = false,
///    set_status_panel_generation = seed+1, require_identity)` — under ONE flock
///    it overwrites the OLD panel id AND bumps the generation epoch, but only
///    when the row still belongs to THIS turn (`Bound`). A non-`Bound` outcome
///    means the row changed turns / disappeared / IO failed: the on-disk row was
///    NOT advanced (no partial re-anchor), so the just-sent NEW panel is discarded
///    (durable orphan on transient delete failure) and the OLD panel + epoch are
///    kept.
/// 3. On `Bound`: retire the stranded OLD panel above the answer (durable orphan
///    on transient delete failure) and adopt the new id + epoch into the loop
///    locals so this turn's own completion proves the SAME (new) epoch while a
///    stale OLD-epoch completion for the re-anchored panel is stale-skipped.
///
/// Pure msg-id / HTTP bookkeeping — the per-channel `StatusPanelState` is never
/// torn down, so item4's `session_banner` exactly-once claim is untouched. No
/// live panel (`status_panel_msg_id.is_none()`) → no-op returning `false`.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn reanchor_watcher_two_message_status_panel_below_answer(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    tmux_session_name: &str,
    require_identity: Option<crate::services::discord::inflight::InflightTurnIdentity>,
    panel_text: &str,
    status_panel_msg_id: &mut Option<serenity::MessageId>,
    this_turn_status_panel_generation: &mut u64,
    last_status_panel_text: &mut String,
) -> bool {
    let Some(old_panel_id) = *status_panel_msg_id else {
        return false;
    };
    let next_generation = this_turn_status_panel_generation.saturating_add(1);

    rate_limit_wait(shared, channel_id).await;
    let new_panel =
        match crate::services::discord::http::send_channel_message(http, channel_id, panel_text)
            .await
        {
            Ok(message) => message,
            Err(error) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ watcher: #3805 P2 re-anchor panel send failed in channel {}: {}",
                    channel_id.get(),
                    error
                );
                return false;
            }
        };

    let bind_outcome = crate::services::discord::inflight::bind_status_panel(
        provider,
        channel_id.get(),
        new_panel.id.get(),
        &crate::services::discord::inflight::StatusPanelBindGuard {
            require_identity,
            skip_if_panel_already_set: false,
            set_status_panel_generation: Some(next_generation),
            ..Default::default()
        },
    );

    if bind_outcome != crate::services::discord::inflight::StatusPanelBindOutcome::Bound {
        let discard = delete_nonterminal_placeholder(
            http,
            channel_id,
            shared,
            provider,
            tmux_session_name,
            new_panel.id,
            "watcher_two_message_reanchor_bind_unowned",
        )
        .await;
        if !discard.is_committed() && !discard.is_permanent_failure() {
            enqueue_watcher_status_panel_orphan(
                shared.as_ref(),
                provider,
                channel_id,
                new_panel.id,
            );
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ watcher: #3805 P2 re-anchor bind did not record our panel in channel {} (outcome={:?}); kept the prior panel and discarded the duplicate",
            channel_id.get(),
            bind_outcome
        );
        return false;
    }

    let retire = delete_nonterminal_placeholder(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        old_panel_id,
        "watcher_two_message_reanchor_old_panel",
    )
    .await;
    if !retire.is_committed() && !retire.is_permanent_failure() {
        enqueue_watcher_status_panel_orphan(shared.as_ref(), provider, channel_id, old_panel_id);
    }
    *status_panel_msg_id = Some(new_panel.id);
    *this_turn_status_panel_generation = next_generation;
    *last_status_panel_text = panel_text.to_string();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_gate_off_is_byte_identical_true_regardless_of_answer() {
        // OFF: the block runs exactly as today whether or not the answer exists.
        assert!(watcher_two_message_panel_creation_gated_by_answer(
            false, false
        ));
        assert!(watcher_two_message_panel_creation_gated_by_answer(
            false, true
        ));
    }

    #[test]
    fn creation_gate_on_defers_until_answer_placeholder_exists() {
        // ON: no answer placeholder yet → defer (create the panel next interval,
        // below the answer). Answer placeholder present → proceed (panel below).
        assert!(!watcher_two_message_panel_creation_gated_by_answer(
            true, false
        ));
        assert!(watcher_two_message_panel_creation_gated_by_answer(
            true, true
        ));
    }

    #[test]
    fn bind_generation_off_is_none_on_opens_next_epoch() {
        // OFF → None (the bind guard leaves the generation untouched).
        assert_eq!(watcher_two_message_bind_generation(false, 0), None);
        assert_eq!(watcher_two_message_bind_generation(false, 5), None);
        // ON → seed + 1 (parity with the sink create's saturating_add(1)).
        assert_eq!(watcher_two_message_bind_generation(true, 0), Some(1));
        assert_eq!(watcher_two_message_bind_generation(true, 5), Some(6));
        assert_eq!(
            watcher_two_message_bind_generation(true, u64::MAX),
            Some(u64::MAX)
        );
    }

    fn on_disk(status_message_id: Option<u64>, status_panel_generation: u64) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            777,
            None,
            1,
            7_000_001,
            42,
            "hello".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        state.status_message_id = status_message_id;
        state.status_panel_generation = status_panel_generation;
        state
    }

    #[test]
    fn completion_guard_off_and_equal_epoch_are_inert() {
        let _env = isolate_agentdesk_runtime_root_for_two_message_tests();
        let panel = serenity::MessageId::new(20);
        // Default-OFF / PR-C: this-turn epoch equals the on-disk epoch for the
        // owned panel → never superseded (0 == 0 and 1 == 1).
        assert!(!watcher_two_message_status_completion_superseded(
            0,
            Some(panel),
            Some(&on_disk(Some(20), 0)),
        ));
        assert!(!watcher_two_message_status_completion_superseded(
            1,
            Some(panel),
            Some(&on_disk(Some(20), 1)),
        ));
    }

    #[test]
    fn completion_guard_supersedes_only_newer_epoch_for_owned_panel() {
        let _env = isolate_agentdesk_runtime_root_for_two_message_tests();
        let panel = serenity::MessageId::new(20);
        // Newer on-disk epoch for the SAME owned panel → superseded (PR-D/E use).
        assert!(watcher_two_message_status_completion_superseded(
            1,
            Some(panel),
            Some(&on_disk(Some(20), 2)),
        ));
        // On-disk row owns a DIFFERENT panel → not owned by this turn → never
        // superseded, even with a higher epoch.
        assert!(!watcher_two_message_status_completion_superseded(
            1,
            Some(panel),
            Some(&on_disk(Some(99), 9)),
        ));
        // No panel handle / no on-disk row → nothing to supersede.
        assert!(!watcher_two_message_status_completion_superseded(
            1,
            None,
            Some(&on_disk(Some(20), 9)),
        ));
        assert!(!watcher_two_message_status_completion_superseded(
            1,
            Some(panel),
            None
        ));
    }

    #[test]
    fn completion_guard_ignores_synthetic_headless_panel_handle() {
        let _env = isolate_agentdesk_runtime_root_for_two_message_tests();
        // A synthetic-headless id is not a real Discord message → owns no panel,
        // so a higher on-disk epoch never suppresses this completion.
        let synthetic = serenity::MessageId::new(9_100_000_000_000_000_123);
        assert!(crate::services::discord::is_synthetic_headless_message_id_raw(synthetic.get()));
        assert!(!watcher_two_message_status_completion_superseded(
            1,
            Some(synthetic),
            Some(&on_disk(Some(synthetic.get()), 9)),
        ));
    }

    #[test]
    fn reanchor_bind_bumps_epoch_atomically_and_guard_stale_skips_old_epoch() {
        // #3805 P2 (PR-D) watcher CAS parity: the re-anchor's atomic rebind
        // (`bind_status_panel` with `set_status_panel_generation`) overwrites the
        // OLD panel id AND bumps the epoch under ONE flock. The completion guard
        // then stale-skips a completion carrying the OLD epoch for the re-anchored
        // (owned) panel, while this turn's own completion at the NEW epoch passes.
        let _env = isolate_agentdesk_runtime_root_for_two_message_tests();
        let provider = ProviderKind::Claude;
        let old_panel = serenity::MessageId::new(20);
        let new_panel = serenity::MessageId::new(40);

        // Persist a row that already owns the OLD panel at epoch 1 (as if the
        // watcher created the two-message panel this turn).
        let created = on_disk(Some(old_panel.get()), 1);
        let channel_id = created.channel_id;
        crate::services::discord::inflight::save_inflight_state(&created)
            .expect("persist inflight");

        // Simulate the PR-D re-anchor write: rebind to the NEW panel id + bump the
        // epoch (1 → 2) under the flock, overwriting the OLD id (skip = false).
        let outcome = crate::services::discord::inflight::bind_status_panel(
            &provider,
            channel_id,
            new_panel.get(),
            &crate::services::discord::inflight::StatusPanelBindGuard {
                skip_if_panel_already_set: false,
                set_status_panel_generation: Some(2),
                ..Default::default()
            },
        );
        assert_eq!(
            outcome,
            crate::services::discord::inflight::StatusPanelBindOutcome::Bound
        );

        let after = crate::services::discord::inflight::load_inflight_state(&provider, channel_id)
            .expect("reload inflight");
        // The CAS store now owns the NEW panel at the bumped epoch.
        assert_eq!(after.status_message_id, Some(new_panel.get()));
        assert_eq!(after.status_panel_generation, 2);

        // A stale completion at the OLD epoch (1) for the re-anchored (owned)
        // panel is stale-skipped ("이전 위치 stale-skip").
        assert!(watcher_two_message_status_completion_superseded(
            1,
            Some(new_panel),
            Some(&after),
        ));
        // This turn's own completion at the NEW epoch (2) passes ("새 위치 통과").
        assert!(!watcher_two_message_status_completion_superseded(
            2,
            Some(new_panel),
            Some(&after),
        ));
        // A stale completion still pointing at the OLD (now-retired) panel is not
        // gated by the epoch (the row no longer owns it) — the delete/orphan path
        // handles it, not the generation guard.
        assert!(!watcher_two_message_status_completion_superseded(
            1,
            Some(old_panel),
            Some(&after),
        ));
    }

    /// #3293: `InflightTurnState::new` resolves the AgentDesk runtime store, which
    /// panics unless the runtime root is a tempdir (never the live `~/.adk/release`).
    /// Point `AGENTDESK_ROOT_DIR` at a throwaway dir under the shared env lock so
    /// constructing a test inflight is deterministic; restore on drop.
    struct RuntimeRootGuard {
        previous: Option<std::ffi::OsString>,
        _root: tempfile::TempDir,
    }

    impl Drop for RuntimeRootGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn isolate_agentdesk_runtime_root_for_two_message_tests()
    -> (std::sync::MutexGuard<'static, ()>, RuntimeRootGuard) {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
        (
            lock,
            RuntimeRootGuard {
                previous,
                _root: root,
            },
        )
    }
}
