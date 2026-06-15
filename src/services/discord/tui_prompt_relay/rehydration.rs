//! #3479 rank-10: the Claude TUI binding REHYDRATION + dead/orphaned-session
//! eviction pass. These functions run off the Tokio executor (the async caller
//! dispatches the whole pass via `spawn_blocking`) and self-heal the
//! tmux-session→channel registry / dedupe mirror for sessions that survived a
//! compact/restart/rebind but lost their watcher slot.
//!
//! They are Discord-IO/`SharedData`-COUPLED but cohesive: every dependency
//! (SharedData methods, the sibling launch-script/offset/channel helpers, and
//! the dedupe/platform/tmux_diagnostics modules) is reached via the `use
//! super::*;` glob, so the move is behavior-identical and the parent's call
//! sites (and the `#[cfg(test)] mod tests` block) stay byte-identical via the
//! `use self::rehydration::{...}` re-import.

use super::*;

/// #3105 (codex P1 sub-case B): a tmux session whose dedupe mirror still holds a
/// stale ClaudeTui binding but which is genuinely dead/orphaned — pane gone AND no
/// LIVE watcher handle owns it — under which it is safe to drop the restored-owner
/// binding and tombstone the mirror. EXCLUDES sub-case A (a live session whose
/// registry entry was transiently evicted: pane still live → returns false →
/// self-heals via `restore_owner_channel_for_tmux_session`). A
/// `restored_owner_by_tmux_session` entry is NOT proof of life — it is the stale
/// residue this eviction reclaims. P2: eviction is destructive, so the dead verdict
/// resists a flake — the pane must read dead across `DEAD_ORPHANED_PANE_PROBE_
/// SAMPLES` consecutive samples AND the hard `tmux_session_exists` must confirm the
/// session is gone (a single soft "no live pane" read can NEVER evict).
#[cfg(unix)]
pub(super) fn claude_tui_session_is_dead_orphaned(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> bool {
    // A live watcher handle is conclusive proof of life: never evict, never probe.
    if shared
        .tmux_watchers
        .has_live_watcher_handle(tmux_session_name)
    {
        return false;
    }
    pane_is_confirmed_dead_orphaned(
        || crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name),
        || crate::services::tmux_diagnostics::tmux_session_exists(tmux_session_name),
        DEAD_ORPHANED_PANE_PROBE_SAMPLES,
        Some(DEAD_ORPHANED_PANE_PROBE_DELAY),
    )
}

/// #3105 (codex P2): pure, testable core of the dead/orphaned pane decision (the
/// caller short-circuits on a live watcher handle first). Conservative so a LIVE
/// session is NEVER classified dead from a flake: (1) ANY of up to `samples`
/// `has_live_pane` reads being live ⇒ NOT dead (self-heal preserved); only all-dead
/// proceeds. (2) Even then the hard `session_exists` (`tmux has-session`) must
/// confirm the session is gone. Sub-case B (a genuinely-gone session) reads dead on
/// every sample AND `session_exists` is false ⇒ true ⇒ the WARN spam stops.
#[cfg(unix)]
pub(super) fn pane_is_confirmed_dead_orphaned(
    mut has_live_pane: impl FnMut() -> bool,
    session_exists: impl FnOnce() -> bool,
    samples: usize,
    inter_probe_delay: Option<Duration>,
) -> bool {
    let samples = samples.max(1);
    for sample in 0..samples {
        if has_live_pane() {
            // Any live observation across the window means the session is alive
            // (or recovered from a flake): never evict.
            return false;
        }
        if sample + 1 < samples {
            if let Some(delay) = inter_probe_delay {
                // Blocking sleep is intentional here: this sync core (and the
                // sync `tmux` subprocess probes it drives) only ever runs off
                // the Tokio executor — the sole async caller dispatches the
                // whole rehydrate pass via `spawn_blocking` (#3105 codex P2), so
                // this never stalls an executor worker.
                std::thread::sleep(delay);
            }
        }
    }
    // Every soft probe agreed the pane is dead. Require the hard has-session check
    // to confirm the session truly does not exist before declaring it orphaned.
    !session_exists()
}

/// #3105 (codex P1 sub-case B): evict the stale dedupe mirror for every Claude
/// TUI runtime binding whose session is dead/orphaned, BEFORE the idle relay
/// loop iterates the mirror. The relay loop's `for` is driven by
/// `runtime_bindings_for_kind(ClaudeTui)`; without this pass a dead/orphaned
/// session (e.g. a thread-suffixed session whose pane no longer exists on this
/// host) is yielded every iteration, fails authoritative owner resolution, and
/// re-emits the drift + skip WARN every ~0.5s indefinitely. Tombstoning the
/// mirror here removes the binding from that iteration set, so the spam stops
/// after a single bounded incident line. A later legitimate re-registration
/// (launch-script rehydrate or a fresh watcher) re-populates the mirror, so a
/// session that comes back relays again.
///
/// This pass is independent of `list_session_names()` (which never contains a
/// session that is gone from this host), which is exactly why the previous
/// in-`list` dead-pane branch could not reach it.
#[cfg(unix)]
pub(super) fn evict_dead_orphaned_claude_tui_mirrors(shared: &Arc<SharedData>) {
    for (tmux_session_name, _binding) in
        crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(RuntimeHandoffKind::ClaudeTui)
    {
        if !claude_tui_session_is_dead_orphaned(shared, &tmux_session_name) {
            continue;
        }
        // Drop any leftover restored-owner binding (a dead session must never
        // resolve an authoritative owner), then tombstone the dedupe mirror.
        shared
            .tmux_watchers
            .clear_restored_owner_for_tmux_session(&tmux_session_name);
        if crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(&tmux_session_name) {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                provider = "claude",
                "evicted stale dedupe mirror for dead/orphaned Claude TUI session \
                 (pane gone, no live watcher); idle relay will no longer re-emit \
                 per-poll drift/skip warnings for it"
            );
        }
    }
}

#[cfg(unix)]
pub(super) fn rehydrate_existing_claude_tui_bindings(shared: &Arc<SharedData>) {
    // #3105 (codex P1 sub-case B): tombstone stale mirrors for dead/orphaned
    // sessions BEFORE anything else, so the per-poll drift/skip WARN spam stops
    // even when the session is not present in `list_session_names()` at all.
    evict_dead_orphaned_claude_tui_mirrors(shared);

    let sessions = match crate::services::platform::tmux::list_session_names() {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::debug!(error = %error, "Claude TUI binding rehydrate skipped; tmux sessions unavailable");
            return;
        }
    };

    for tmux_session_name in sessions {
        let existing_binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
            &tmux_session_name,
        );
        // #3018: dedupe lookup here is a diagnostic/mirror rehydration hint only
        // (subordinate to the freshly resolved channel below), never a routing
        // authority — the authoritative resolver is owner_channel_for_tmux_session.
        let existing_channel =
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(&tmux_session_name);
        let fresh_binding = rehydrated_claude_tui_binding_for_tmux_session(&tmux_session_name);
        // #3105: prefer the settings-derived (authoritative) channel; only fall
        // back to the dedupe mirror's last-seen channel for the dedupe binding
        // refresh below. The mirror's value must NOT be promoted into the
        // authoritative registry — see the repair gate below.
        let authoritative_channel = resolve_rehydrated_claude_tmux_channel_id(&tmux_session_name);
        let channel_id = match authoritative_channel.or(existing_channel) {
            Some(channel_id) => channel_id,
            None => continue,
        };
        if !crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux_session_name) {
            // #3105: the restored owner binding is only valid for a LIVE session;
            // drop it once the pane is gone so a dead session can never resolve.
            shared
                .tmux_watchers
                .clear_restored_owner_for_tmux_session(&tmux_session_name);
            // #3105 (codex P1 sub-case B): a listed-but-dead pane with no live
            // watcher is orphaned — also tombstone its stale dedupe mirror so the
            // idle relay loop stops re-emitting the per-poll drift/skip WARN for
            // it (the dead-pane branch previously only cleared the restored owner
            // and left the mirror to spam). `clear_restored_owner_*` above already
            // ran, so the dead-orphaned predicate cannot be masked by a stale
            // restored owner here.
            if claude_tui_session_is_dead_orphaned(shared, &tmux_session_name)
                && crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(&tmux_session_name)
            {
                tracing::warn!(
                    tmux_session_name = %tmux_session_name,
                    provider = "claude",
                    "evicted stale dedupe mirror for dead/orphaned Claude TUI session \
                     (listed pane dead, no live watcher)"
                );
            }
            continue;
        }
        // #3105: self-heal the authoritative tmux-session→channel registry for a
        // LIVE Claude TUI session that has no live watcher handle (e.g. the slot
        // was evicted by a compact/restart/rebind and never re-claimed because
        // the user is typing directly into the pane). Without this the #3018
        // "registry is the single authority, never fall back to the mirror" rule
        // turns a transient registry miss into a PERMANENT relay drop. We promote
        // ONLY the settings-derived channel (authoritative, resolves both base
        // and thread-suffixed tmux names) — never the dedupe mirror — and emit a
        // single bounded incident instead of the per-poll drift warning.
        if let Some(authoritative_channel) = authoritative_channel {
            let repaired = shared.tmux_watchers.restore_owner_channel_for_tmux_session(
                &tmux_session_name,
                ChannelId::new(authoritative_channel),
            );
            if repaired {
                tracing::warn!(
                    tmux_session_name = %tmux_session_name,
                    channel_id = authoritative_channel,
                    provider = "claude",
                    "repaired authoritative tmux-session→channel registry for live TUI session \
                     (no live watcher slot); idle relay can route again"
                );
            }
        }
        if let (Some(existing), Some(_)) = (&existing_binding, existing_channel) {
            if existing.runtime_kind == RuntimeHandoffKind::ClaudeTui
                && Path::new(&existing.output_path).exists()
                && match fresh_binding.as_ref() {
                    Some(fresh) => claude_tui_runtime_binding_matches_launch(existing, fresh),
                    None => true,
                }
            {
                continue;
            }
        }
        if let Some(fresh) = fresh_binding {
            let should_refresh = match existing_binding.as_ref() {
                Some(existing) => {
                    !claude_tui_runtime_binding_matches_launch(existing, &fresh)
                        || !Path::new(&existing.output_path).exists()
                }
                None => true,
            };
            if should_refresh {
                crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
                    ProviderKind::Claude.as_str(),
                    &tmux_session_name,
                    channel_id,
                    fresh.clone(),
                );
                tracing::info!(
                    tmux_session_name = %tmux_session_name,
                    channel_id,
                    transcript_path = %fresh.output_path,
                    last_offset = fresh.last_offset,
                    "rehydrated Claude TUI direct relay binding from launch script"
                );
                continue;
            }
        }
        if let Some(binding) = existing_binding {
            if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui {
                continue;
            }
            if Path::new(&binding.output_path).exists() {
                crate::services::tui_prompt_dedupe::register_rehydrated_tmux_runtime_binding(
                    ProviderKind::Claude.as_str(),
                    &tmux_session_name,
                    channel_id,
                    binding.clone(),
                );
                tracing::info!(
                    tmux_session_name = %tmux_session_name,
                    channel_id,
                    transcript_path = %binding.output_path,
                    last_offset = binding.last_offset,
                    "rehydrated Claude TUI direct relay channel binding"
                );
            }
            continue;
        }
    }
}

#[cfg(unix)]
pub(super) fn rehydrated_claude_tui_binding_for_tmux_session(
    tmux_session_name: &str,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    let launch_script_path = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
    )?;
    let launch = parse_claude_tui_launch_script(Path::new(&launch_script_path)).ok()?;
    let transcript_path = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        &launch.working_dir,
        &launch.session_id,
        None,
    )
    .ok()?;
    if !transcript_path.exists() {
        return None;
    }
    let start_offset = claude_tui_rehydrate_start_offset(&transcript_path);
    Some(crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: transcript_path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(launch.session_id),
        last_offset: start_offset,
        relay_last_offset: None,
    })
}
