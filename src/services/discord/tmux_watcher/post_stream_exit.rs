use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// #4229 S5: post-stream-exit finalize tail of `tmux_output_watcher_with_restore`
/// (DashMap slot removal, dispatch-protection resolve + dead-session dispatch fail,
/// pane-dead explicit inflight cleanup, dead tmux session kill + post-mortem capture,
/// idle-status report, watcher-stopped log), moved verbatim from tmux_watcher.rs.
pub(super) struct PostStreamExitContext {
    pub(super) channel_id: ChannelId,
    pub(super) shared: Arc<SharedData>,
    pub(super) tmux_session_name: String,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) watcher_turn_identity:
        Option<crate::services::discord::inflight::InflightTurnIdentity>,
    pub(super) watcher_instance_id: u64,
}

pub(super) async fn run_post_stream_exit(ctx: PostStreamExitContext) {
    let PostStreamExitContext {
        channel_id,
        shared,
        tmux_session_name,
        cancel,
        watcher_turn_identity,
        watcher_instance_id,
    } = ctx;

    // Cleanup: release this watcher's registry slot.
    // #243: When a watcher is cancelled (replaced by a new watcher or shutdown),
    // the replacement already occupies the slot — removing would delete the new entry.
    release_registry_slot_at_exit(
        &shared.tmux_watchers,
        channel_id,
        &tmux_session_name,
        &cancel,
    );

    let api_port = shared.api_port;
    let provider = shared.settings.read().await.provider.clone();
    let session_key = crate::services::discord::adk_session::build_adk_session_key(
        &shared, channel_id, &provider, None,
    )
    .await;
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.clone())
    };
    let dispatch_protection =
        crate::services::discord::tmux_lifecycle::resolve_dispatch_tmux_protection(
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            &provider,
            &tmux_session_name,
            channel_name.as_deref(),
        );
    let dispatch_failed_for_dead_session = if let Some(protection) = dispatch_protection.as_ref() {
        crate::services::discord::tmux_lifecycle::fail_active_dispatch_for_dead_tmux_session(
            api_port,
            protection,
            &tmux_session_name,
            "tmux_watcher",
        )
        .await
    } else {
        false
    };
    let cleanup_plan = dead_session_cleanup_plan(
        dispatch_protection.is_some() && !dispatch_failed_for_dead_session,
    );

    if let Some(protection) = dispatch_protection {
        let ts = chrono::Local::now().format("%H:%M:%S");
        if dispatch_failed_for_dead_session {
            tracing::warn!(
                "  [{ts}] tmux watcher: failed active dispatch for dead session {} — {}",
                tmux_session_name,
                protection.log_reason()
            );
        } else {
            tracing::info!(
                "  [{ts}] ♻ tmux watcher: preserving dispatch session {} — {}",
                tmux_session_name,
                protection.log_reason()
            );
        }
    }

    if !cleanup_plan.preserve_tmux_session {
        // #2427 A wire: pane-death explicit inflight cleanup. The
        // tmux pane is gone (or about to be killed below), so any
        // inflight row still pointing at this provider/channel will
        // never receive a normal completion hook. Without this the
        // sweeper has to time-guess (`STALL`/`ABANDON`) before evicting,
        // reproducing the #2415 family of "completion-missing → time
        // heuristic" bugs.
        //
        // We re-check `tmux_session_has_live_pane` on the blocking
        // thread before clearing, matching the same revalidation the
        // kill path uses (#1261 codex P2) so a concurrent
        // `start_claude` respawn of a fresh same-named session does not
        // get its inflight wiped.
        {
            let sess_for_inflight = tmux_session_name.clone();
            let provider_for_inflight = provider.clone();
            let channel_id_inflight = channel_id;
            let watcher_identity_for_inflight = watcher_turn_identity.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let pane_alive = tmux_session_has_live_pane(&sess_for_inflight);
                if pane_alive {
                    // Pane resurrected (e.g. start_claude respawn race) —
                    // do not touch its inflight.
                    return;
                }
                emit_explicit_inflight_cleanup_signal_pane_dead(
                    &provider_for_inflight,
                    channel_id_inflight,
                    &sess_for_inflight,
                    watcher_identity_for_inflight.as_ref(),
                );
            })
            .await;
        }

        // Kill dead tmux session to prevent accumulation (especially for thread sessions
        // which are created per-dispatch and would otherwise linger for 24h).
        // #145: skip kill for unified-thread sessions with active auto-queue runs.
        {
            let sess = tmux_session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if tmux_session_exists(&sess) && !tmux_session_has_live_pane(&sess) {
                    // Check if this is a unified-thread session before killing
                    if let Some((_, ch_name)) =
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(&sess)
                    {
                        if crate::dispatch::is_unified_thread_channel_name_active(&ch_name) {
                            return;
                        }
                    }
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "dead_after_turn",
                        Some("watcher cleanup: dead session after turn"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: dead session after turn");

                    // #1261 (Fix B): the wrapper's stderr `[stderr] ...` lines and
                    // synthetic `[fatal startup error]` markers go to the PTY, not
                    // to the structured jsonl that `recent_output_tail` reads. Dump
                    // the current pane buffer to a `death_pane_log` file BEFORE we
                    // kill the session so the wrapper-level death context is still
                    // recoverable post-mortem. Kept out of `cleanup_session_temp_files`
                    // EXTS on purpose — the file persists past the cleanup and is
                    // overwritten on the next death of the same session.
                    if let Some(pane_content) =
                        crate::services::platform::tmux::capture_pane(&sess, -1000)
                    {
                        let stamped = format!(
                            "[{}] post-mortem capture for session={}\n{}",
                            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                            sess,
                            pane_content
                        );
                        let path = crate::services::tmux_common::session_temp_path(
                            &sess,
                            "death_pane_log",
                        );
                        if let Some(parent) = std::path::Path::new(&path).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(&path, stamped);
                    }

                    // #1261 (codex P2): the `capture_pane` subprocess above
                    // widens the gap between the outer dead-pane gate and the
                    // kill. In that window a concurrent follow-up could run
                    // claude.rs::start_claude, which kills the stale session
                    // (line 1294), respawns a fresh live session with the
                    // same name (line 1379), and we'd then kill the brand-new
                    // session here. Revalidate the dead-pane condition right
                    // before the kill so we only tear down the same
                    // dead-paned session we capture-paned.
                    if tmux_session_exists(&sess) && !tmux_session_has_live_pane(&sess) {
                        crate::services::platform::tmux::kill_session(
                            &sess,
                            "watcher cleanup: dead session after turn",
                        );
                    }
                    // NOTE: jsonl/FIFO/etc. cleanup intentionally NOT done here.
                    // `claude.rs::start_claude` calls
                    // `cleanup_session_temp_files` at spawn time
                    // (`claude.rs:1304`) before recreating the canonical paths,
                    // which already covers the "next-spawn against stale jsonl"
                    // case. Pairing a watcher-side cleanup with the kill races
                    // with that spawn-side cleanup + recreate (#1261 codex P1):
                    // if the next message lands between our `kill_session` and
                    // our cleanup, claude's spawn already laid down fresh files
                    // and our cleanup deletes them, breaking the new turn.
                    // Keep cleanup as a single-source-of-truth on the spawn
                    // path.
                }
            })
            .await;
        }
    }

    let defer_idle_status_to_bridge =
        crate::services::discord::inflight::load_inflight_state(&provider, channel_id.get())
            .as_ref()
            .is_some_and(|state| {
                state.tmux_session_name.as_deref() == Some(tmux_session_name.as_str())
            });

    if cleanup_plan.report_idle_status && !defer_idle_status_to_bridge {
        // Report idle status to DB so the dashboard doesn't show stale "working" state.
        // Always report idle when the watcher exits, even if dispatch protection
        // keeps the dead tmux session around for the active-dispatch safety path.
        let thread_channel_id = channel_name
            .as_deref()
            .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name);
        let agent_id = resolve_role_binding(channel_id, channel_name.as_deref())
            .map(|binding| binding.role_id);
        crate::services::discord::adk_session::post_adk_session_status(
            session_key.as_deref(),
            channel_name.as_deref(),
            None, // model
            "idle",
            &provider,
            None, // session_info
            None, // tokens
            None, // cwd
            None, // dispatch_id
            thread_channel_id,
            Some(channel_id),
            agent_id.as_deref(),
            api_port,
        )
        .await;
    } else if cleanup_plan.report_idle_status {
        tracing::debug!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            tmux_session = %tmux_session_name,
            "watcher deferred idle status because bridge-owned inflight still needs terminal Discord finalization"
        );
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 👁 tmux watcher stopped for #{tmux_session_name} (instance {watcher_instance_id})"
    );
}

/// #5071 T3-A2: drop this watcher's live registry slot at post-stream exit, but
/// only while the registered handle is still this watcher's own.
///
/// The previous `!cancel` boolean was not that proof. A replacement claims the
/// slot through `insert_locked`, which never touches the outgoing watcher's
/// cancel flag, so a late stale exit could read `false` here and remove the new
/// entry by channel key — exactly the #243 hazard the boolean was meant to
/// avoid. `remove_tmux_session_if_current` compares the registered handle's
/// cancel `Arc` pointer under the registry lock instead, so a replaced slot is
/// preserved.
///
/// This is a duplicate, not new machinery: the `Drop` guard
/// (`task_supervisor::TmuxWatcherTaskGuard`) already makes the identical helper
/// call immediately after this function returns, and T3-A2 places that same
/// call at the post-stream exit once more.
fn release_registry_slot_at_exit(
    tmux_watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    tmux_session_name: &str,
    cancel: &Arc<AtomicBool>,
) {
    let Some((owner_channel_id, _handle)) =
        tmux_watchers.remove_tmux_session_if_current(tmux_session_name, cancel)
    else {
        return;
    };
    tracing::debug!(
        channel_id = channel_id.get(),
        owner_channel_id = owner_channel_id.get(),
        tmux_session_name,
        "post-stream exit removed its own watcher registry entry"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watcher_handle(tmux_session_name: &str) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: format!("/tmp/{tmux_session_name}.jsonl"),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                crate::services::discord::tmux_watcher_now_ms(),
            )),
        }
    }

    fn registered_cancel(
        registry: &TmuxWatcherRegistry,
        tmux_session_name: &str,
    ) -> Option<Arc<AtomicBool>> {
        registry
            .by_tmux_session
            .get(tmux_session_name)
            .map(|entry| entry.cancel.clone())
    }

    // #5071 T3-A2 mutation gate: an old watcher that reaches post-stream exit
    // AFTER its slot was re-claimed must leave the new handle alone. Reverting
    // the body of `release_registry_slot_at_exit` to the channel-keyed
    // `tmux_watchers.remove(&channel_id)` kills this test.
    #[test]
    fn stale_post_stream_exit_preserves_a_replacement_watcher_entry() {
        let registry = TmuxWatcherRegistry::new();
        let channel = ChannelId::new(1_504_468_805_772_902_471);
        let tmux = "AgentDesk-claude-adk-cc";

        let old_handle = watcher_handle(tmux);
        let old_cancel = old_handle.cancel.clone();
        registry.insert(channel, old_handle);

        // The replacement claims the same slot through `insert_locked` without
        // ever setting the outgoing watcher's cancel flag.
        let new_handle = watcher_handle(tmux);
        let new_cancel = new_handle.cancel.clone();
        registry.insert(channel, new_handle);
        assert!(!old_cancel.load(std::sync::atomic::Ordering::Relaxed));

        release_registry_slot_at_exit(&registry, channel, tmux, &old_cancel);

        let still_registered = registered_cancel(&registry, tmux)
            .expect("the replacement watcher entry must survive the stale exit");
        assert!(
            Arc::ptr_eq(&still_registered, &new_cancel),
            "the stale exit must not evict the replacement handle"
        );
        assert_eq!(registry.owner_channel_for_tmux_session(tmux), Some(channel));
    }

    // The same stale exit must also keep a slot the channel was rebound to
    // under a different tmux session name — the channel-keyed remove tore that
    // entry down even though it never belonged to the exiting watcher.
    #[test]
    fn stale_post_stream_exit_preserves_a_rebound_channel_entry() {
        let registry = TmuxWatcherRegistry::new();
        let channel = ChannelId::new(4794);
        let old_tmux = "AgentDesk-claude-adk-cc";
        let new_tmux = "AgentDesk-claude-adk-cc-t4794";

        let old_handle = watcher_handle(old_tmux);
        let old_cancel = old_handle.cancel.clone();
        registry.insert(channel, old_handle);

        let new_handle = watcher_handle(new_tmux);
        let new_cancel = new_handle.cancel.clone();
        registry.insert(channel, new_handle);

        release_registry_slot_at_exit(&registry, channel, old_tmux, &old_cancel);

        let still_registered = registered_cancel(&registry, new_tmux)
            .expect("the rebound session's entry must survive the stale exit");
        assert!(Arc::ptr_eq(&still_registered, &new_cancel));
        assert_eq!(
            registry.owner_channel_for_tmux_session(new_tmux),
            Some(channel)
        );
    }

    // The non-stale case still releases the slot, so the guard is a fence and
    // not a blanket no-op: a mutation that deletes the helper call survives the
    // two tests above but dies here.
    #[test]
    fn post_stream_exit_releases_its_own_registry_entry() {
        let registry = TmuxWatcherRegistry::new();
        let channel = ChannelId::new(5071);
        let tmux = "AgentDesk-codex-adk-cc";

        let handle = watcher_handle(tmux);
        let cancel = handle.cancel.clone();
        registry.insert(channel, handle);

        release_registry_slot_at_exit(&registry, channel, tmux, &cancel);

        assert!(registered_cancel(&registry, tmux).is_none());
        assert_eq!(registry.owner_channel_for_tmux_session(tmux), None);
        assert!(!registry.contains_key(&channel));
    }
}
