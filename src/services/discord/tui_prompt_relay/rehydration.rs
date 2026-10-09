//! #3479 rank-10: the Claude TUI binding REHYDRATION + dead/orphaned-session
//! eviction pass. These functions run off the Tokio executor (the async caller
//! dispatches the whole pass via `spawn_blocking`) and self-heal the
//! tmux-session→channel registry / dedupe mirror for sessions that survived a
//! compact/restart/rebind but lost their watcher slot.
//!
//! They are Discord-IO/`SharedData`-COUPLED but cohesive: every dependency but the launch
//! transcript helpers (SharedData methods, sibling launch-script/offset/channel helpers, and
//! the dedupe/platform/tmux_diagnostics modules) is reached via the `use
//! super::*;` glob, so the move is behavior-identical and the parent's call
//! sites (and the `#[cfg(test)] mod tests` block) stay byte-identical via the
//! `use self::rehydration::{...}` re-import.

use super::super::host_defer_gate::mirror_evict_admitted as host_admits;
use super::super::recovery_engine::host_reconcile::names_another_host;
#[cfg(unix)]
pub(super) use super::launch_script::rehydrated_claude_tui_binding_for_tmux_session;
use super::launch_script::{
    claude_launch_observation, claude_launch_transcript, claude_tui_rehydrated_binding,
};
use super::*;
use crate::services::tui_prompt_dedupe::pane_registration::{
    register_judged_claude_pane, register_unmoved_claude_pane,
};
use std::collections::HashMap;

#[cfg(unix)]
mod codex_marker;
#[cfg(unix)]
use codex_marker::{CodexTuiMarkerRehydrateDecision, codex_tui_marker_rehydrate_decision};

#[cfg(any(not(test), not(unix)))]
use super::resolve_rehydrated_claude_tmux_channel_id as claude_channel;
#[cfg(any(not(test), not(unix)))]
use crate::services::{
    platform::tmux::list_session_names as claude_session_names,
    tmux_diagnostics::tmux_session_has_live_pane as claude_pane_live,
};
#[cfg(all(test, unix))]
use claude_pass_tests::{claude_channel, claude_pane_live, claude_session_names};
#[cfg(test)]
#[path = "../../claude_tui/hook_server/rehydration_ingress_tests.rs"]
pub(super) mod claude_pass_tests;

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
        || claude_pane_live(tmux_session_name),
        || crate::services::tmux_diagnostics::tmux_session_pane_liveness(tmux_session_name),
        DEAD_ORPHANED_PANE_PROBE_SAMPLES,
        Some(DEAD_ORPHANED_PANE_PROBE_DELAY),
    ) && host_admits(shared, &ProviderKind::Claude, tmux_session_name)
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
    session_liveness: impl FnOnce() -> crate::services::platform::tmux::PaneLiveness,
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
    // Every soft probe agreed the pane is dead. Require the three-state hard
    // probe to confirm death; transport/permission failures preserve the mirror.
    matches!(
        session_liveness(),
        crate::services::platform::tmux::PaneLiveness::DeadOrAbsent
    )
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
        // #5188 (P1-A): the adopted-session authority is scoped to the PANE, so
        // confirmed pane death is the one event that retires it. This is the
        // only caller — the settle pass must never reach it, or the authority
        // goes back to expiring on a ~500ms timer.
        crate::services::tui_prompt_dedupe::forget_hook_adopted_claude_session_id(
            &tmux_session_name,
        );
        let mirror_channel =
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(&tmux_session_name)
                .unwrap_or(0);
        let permanent_loss_count =
            super::super::idle_relay_drift::record_confirmed_dead_orphan_loss(
                &ProviderKind::Claude,
                &tmux_session_name,
                mirror_channel,
            );
        if crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(&tmux_session_name) {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                provider = "claude",
                permanent_loss_count,
                "evicted stale dedupe mirror for dead/orphaned Claude TUI session \
                 (pane gone, no live watcher); idle relay will no longer re-emit \
                 per-poll drift/skip warnings for it"
            );
        }
    }
}

#[cfg(unix)]
fn codex_tui_session_is_dead_orphaned(shared: &Arc<SharedData>, tmux_session_name: &str) -> bool {
    if shared
        .tmux_watchers
        .has_live_watcher_handle(tmux_session_name)
    {
        return false;
    }
    pane_is_confirmed_dead_orphaned(
        || codex_pass_pane_is_live(tmux_session_name),
        || crate::services::tmux_diagnostics::tmux_session_pane_liveness(tmux_session_name),
        DEAD_ORPHANED_PANE_PROBE_SAMPLES,
        Some(DEAD_ORPHANED_PANE_PROBE_DELAY),
    ) && host_admits(shared, &ProviderKind::Codex, tmux_session_name)
}

#[cfg(all(unix, test))]
thread_local! {
    /// Live sessions a test pins for the Codex periodic pass on this thread.
    static CODEX_PASS_TMUX_VIEW: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
fn codex_pass_tmux_session_names() -> Result<Vec<String>, String> {
    #[cfg(test)]
    if let Some(sessions) = CODEX_PASS_TMUX_VIEW.with_borrow(Clone::clone) {
        return Ok(sessions);
    }
    crate::services::platform::tmux::list_session_names()
}

#[cfg(unix)]
fn codex_pass_pane_is_live(tmux_session_name: &str) -> bool {
    #[cfg(test)]
    if let Some(sessions) = CODEX_PASS_TMUX_VIEW.with_borrow(Clone::clone) {
        return sessions.iter().any(|session| session == tmux_session_name);
    }
    crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name)
}

#[cfg(unix)]
fn evict_dead_orphaned_codex_tui_mirrors(shared: &Arc<SharedData>) {
    for (tmux_session_name, _binding) in
        crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(RuntimeHandoffKind::CodexTui)
    {
        if !codex_tui_session_is_dead_orphaned(shared, &tmux_session_name) {
            continue;
        }
        shared
            .tmux_watchers
            .clear_restored_owner_for_tmux_session(&tmux_session_name);
        let mirror_channel =
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(&tmux_session_name)
                .unwrap_or(0);
        let permanent_loss_count =
            super::super::idle_relay_drift::record_confirmed_dead_orphan_loss(
                &ProviderKind::Codex,
                &tmux_session_name,
                mirror_channel,
            );
        if crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(&tmux_session_name) {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                provider = "codex",
                permanent_loss_count,
                "evicted stale dedupe mirror for dead/orphaned Codex TUI session \
                 (pane gone, no live watcher); idle relay will no longer re-emit \
                 per-poll drift/skip warnings for it"
            );
        }
    }
}

#[cfg(unix)]
pub(super) fn rehydrate_existing_claude_tui_bindings(shared: &Arc<SharedData>) {
    // Deferred hook adoptions go first so this pass sees their binding.
    crate::services::claude_tui::hook_server::retry_deferred_claude_adoptions();
    // #3105 (codex P1 sub-case B): tombstone stale mirrors for dead/orphaned
    // sessions BEFORE anything else, so the per-poll drift/skip WARN spam stops
    // even when the session is not present in `list_session_names()` at all.
    evict_dead_orphaned_claude_tui_mirrors(shared);

    match claude_session_names() {
        Ok(sessions) => {
            for tmux_session_name in sessions {
                rehydrate_claude_tui_pane(shared, &tmux_session_name);
            }
            crate::services::claude_tui::hook_server::mark_boot_discovery_complete();
        }
        Err(error) => {
            tracing::debug!(error = %error, "Claude TUI binding rehydrate skipped; tmux sessions unavailable");
        }
    }
    // Herdr panes are not tmux sessions; their rows name them. No local endpoint reads nothing.
    super::super::recovery_engine::herdr_reader::reconnect_restarted_herdr_panes(
        shared.pg_pool.as_ref(),
        &ProviderKind::Claude,
    );
}

/// One pane of the rehydrate pass; a durable Pending is restored before the binding judgment.
#[cfg(unix)]
fn rehydrate_claude_tui_pane(shared: &Arc<SharedData>, tmux_session_name: &str) {
    let existing_binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name);
    // #3018: dedupe lookup here is a diagnostic/mirror rehydration hint only
    // (subordinate to the freshly resolved channel below), never a routing
    // authority — the authoritative resolver is owner_channel_for_tmux_session.
    let existing_channel =
        crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux_session_name);
    let (fresh_binding, launch_context) = claude_launch_observation(tmux_session_name);
    // #3105: prefer the settings-derived (authoritative) channel; only fall
    // back to the dedupe mirror's last-seen channel for the dedupe binding
    // refresh below. The mirror's value must NOT be promoted into the
    // authoritative registry — see the repair gate below.
    let authoritative_channel = claude_channel(tmux_session_name);
    let channel_id = match authoritative_channel.or(existing_channel) {
        Some(channel_id) if !names_another_host(tmux_session_name) => channel_id,
        _ => return,
    };
    if !claude_pane_live(tmux_session_name) {
        if !host_admits(shared, &ProviderKind::Claude, tmux_session_name) {
            return;
        }
        // #3105: the restored owner binding is only valid for a LIVE session;
        // drop it once the pane is gone so a dead session can never resolve.
        shared
            .tmux_watchers
            .clear_restored_owner_for_tmux_session(tmux_session_name);
        // #3105 (codex P1 sub-case B): a listed-but-dead pane with no live
        // watcher is orphaned — also tombstone its stale dedupe mirror so the
        // idle relay loop stops re-emitting the per-poll drift/skip WARN for
        // it (the dead-pane branch previously only cleared the restored owner
        // and left the mirror to spam). `clear_restored_owner_*` above already
        // ran, so the dead-orphaned predicate cannot be masked by a stale
        // restored owner here.
        if claude_tui_session_is_dead_orphaned(shared, tmux_session_name) {
            let permanent_loss_count =
                super::super::idle_relay_drift::record_confirmed_dead_orphan_loss(
                    &ProviderKind::Claude,
                    tmux_session_name,
                    channel_id,
                );
            if crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(tmux_session_name) {
                tracing::warn!(
                    tmux_session_name = %tmux_session_name,
                    provider = "claude",
                    permanent_loss_count,
                    "evicted stale dedupe mirror for dead/orphaned Claude TUI session \
                     (listed pane dead, no live watcher)"
                );
            }
        }
        return;
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
            tmux_session_name,
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
    // A restore that registered the pane, or found its log corrupt, leaves the launch binding out.
    let launch = claude_launch_transcript(tmux_session_name);
    let restored = crate::services::tui_prompt_dedupe::pending::restore_claude_pane(
        tmux_session_name,
        channel_id,
        launch,
        claude_tui_rehydrated_binding,
    );
    if super::headless::restore_holds(tmux_session_name, restored) {
        return;
    }
    let fresh_binding = super::headless::tui_binding(shared, tmux_session_name, fresh_binding);
    // #5188 (R1): both gates below consult the session id a live hook payload
    // most recently reported for this pane. A `/clear` rotates Claude onto a
    // new transcript while the launch script keeps naming the launch-time
    // UUID forever, so that payload is the only evidence which outranks the
    // stale artifact. Both decisions live in `session_rotation_settle` — they
    // were unreachable from any test while inlined here, and production takes
    // no other path to them.
    if let (Some(existing), Some(_)) = (&existing_binding, existing_channel)
        && super::session_rotation_settle::existing_claude_binding_outranks_launch_script(
            tmux_session_name,
            existing,
            fresh_binding.as_ref(),
        )
    {
        crate::services::claude_tui::hook_server::note_claude_pane_registration(
            tmux_session_name,
            existing.session_id.as_deref(),
            true,
        );
        return;
    }
    if let Some(fresh) = fresh_binding {
        let should_refresh = match existing_binding.as_ref() {
            // Second gate on purpose: the first one requires a resolved
            // `existing_channel`, and a pane whose dedupe mirror has no
            // channel yet would otherwise still be reverted here.
            Some(existing) => super::session_rotation_settle::launch_script_may_replace_binding(
                tmux_session_name,
                existing,
                &fresh,
            ),
            None => true,
        };
        if should_refresh {
            if !register_judged_claude_pane(
                tmux_session_name,
                channel_id,
                fresh.clone(),
                launch_context.as_deref(),
                existing_binding.as_ref(),
            ) {
                return;
            }
            tracing::info!(
                tmux_session_name = %tmux_session_name,
                channel_id,
                transcript_path = %fresh.output_path,
                last_offset = fresh.last_offset,
                "rehydrated Claude TUI direct relay binding from launch script"
            );
            return;
        }
    }
    if let Some(binding) = existing_binding {
        if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui {
            return;
        }
        if Path::new(&binding.output_path).exists() {
            if !register_unmoved_claude_pane(tmux_session_name, channel_id, binding.clone()) {
                return;
            }
            tracing::info!(
                tmux_session_name = %tmux_session_name,
                channel_id,
                transcript_path = %binding.output_path,
                last_offset = binding.last_offset,
                "rehydrated Claude TUI direct relay channel binding"
            );
        }
        return;
    }
}

#[cfg(unix)]
pub(super) fn rehydrate_existing_codex_tui_bindings(shared: &Arc<SharedData>) {
    evict_dead_orphaned_codex_tui_mirrors(shared);
    // Codex Herdr rows, read whether or not tmux answers; off Herdr this reads nothing.
    super::super::recovery_engine::herdr_reader::reconnect_restarted_herdr_panes(
        shared.pg_pool.as_ref(),
        &ProviderKind::Codex,
    );

    let mut sessions = match codex_pass_tmux_session_names() {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::debug!(error = %error, "Codex TUI binding rehydrate skipped; tmux sessions unavailable");
            return;
        }
    };
    sessions.sort_by(|left, right| {
        codex_tui_launch_modified_since(right)
            .cmp(&codex_tui_launch_modified_since(left))
            .then_with(|| left.cmp(right))
    });

    let rehydrate_plan = codex_tui_rehydrate_plan(&sessions);
    let mut claimed_rollout_paths = claimed_codex_tui_rollout_paths();

    for tmux_session_name in sessions {
        if !tmux_session_is_codex_tui(&tmux_session_name) {
            continue;
        }

        let authoritative_channel =
            resolve_rehydrated_tmux_channel_id(&ProviderKind::Codex, &tmux_session_name).or_else(
                || {
                    crate::services::tui_prompt_dedupe::codex_verified_discovered_channel(
                        &tmux_session_name,
                    )
                },
            );
        let Some(channel_id) = authoritative_channel.or_else(|| {
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(&tmux_session_name)
        }) else {
            continue;
        };

        if !codex_pass_pane_is_live(&tmux_session_name) {
            if !host_admits(shared, &ProviderKind::Codex, &tmux_session_name) {
                continue;
            }
            shared
                .tmux_watchers
                .clear_restored_owner_for_tmux_session(&tmux_session_name);
            if codex_tui_session_is_dead_orphaned(shared, &tmux_session_name) {
                let permanent_loss_count =
                    super::super::idle_relay_drift::record_confirmed_dead_orphan_loss(
                        &ProviderKind::Codex,
                        &tmux_session_name,
                        channel_id,
                    );
                if crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(&tmux_session_name) {
                    tracing::warn!(
                        tmux_session_name = %tmux_session_name,
                        provider = "codex",
                        permanent_loss_count,
                        "evicted stale dedupe mirror for dead/orphaned Codex TUI session \
                         (listed pane dead, no live watcher)"
                    );
                }
            }
            continue;
        }

        if let Some(authoritative_channel) = authoritative_channel {
            let Some(repaired) = codex_marker::restore_codex_owner_channel(
                shared,
                &tmux_session_name,
                authoritative_channel,
            ) else {
                continue;
            };
            if repaired {
                tracing::warn!(
                    tmux_session_name = %tmux_session_name,
                    channel_id = authoritative_channel,
                    provider = "codex",
                    "repaired authoritative tmux-session->channel registry for live Codex TUI session \
                     (no live watcher slot); idle rollout relay can route again"
                );
            }
        }

        let Some(fresh) = rehydrate_codex_tui_binding_transaction(
            &tmux_session_name,
            channel_id,
            &claimed_rollout_paths,
            &rehydrate_plan.reserved_rollout_paths,
            &rehydrate_plan.duplicate_marker_paths,
            rehydrate_plan
                .markerless_fallback_allowed_sessions
                .contains(&tmux_session_name),
            || {},
        ) else {
            continue;
        };
        tracing::info!(
            tmux_session_name = %tmux_session_name,
            channel_id,
            rollout_path = %fresh.output_path,
            last_offset = fresh.last_offset,
            "rehydrated Codex TUI direct relay binding from live rollout"
        );
        claimed_rollout_paths.insert(canonical_rollout_claim_path(Path::new(&fresh.output_path)));
    }
}

#[cfg(unix)]
fn rehydrate_codex_tui_binding_transaction(
    tmux_session_name: &str,
    channel_id: u64,
    claimed_rollout_paths: &HashSet<PathBuf>,
    reserved_rollout_paths: &HashSet<PathBuf>,
    duplicate_marker_paths: &HashSet<PathBuf>,
    allow_markerless_cwd_fallback: bool,
    observe_before_register: impl FnOnce(),
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    // Restore durable claims before discovery asks the generic publication gate.
    let recovered = crate::services::tui_prompt_dedupe::recover_discovered_codex_binding(
        tmux_session_name,
        channel_id,
    );
    if !recovered {
        return None;
    }
    crate::services::tui_prompt_dedupe::reconcile_rehydrated_tmux_runtime_binding_with_authority(
        ProviderKind::Codex.as_str(),
        tmux_session_name,
        channel_id,
        observe_before_register,
        |authority, existing| {
            if let Some(existing) = existing
                && existing.runtime_kind == RuntimeHandoffKind::CodexTui
                && Path::new(&existing.output_path).exists()
            {
                let marker = crate::services::codex_tui::session::read_codex_tui_rollout_marker(
                    tmux_session_name,
                )
                .filter(|marker| {
                    // The existing binding reserves its own rollout; only a
                    // different source must pass the foreign-claim check.
                    let own_claim = HashSet::new();
                    let claims = if canonical_rollout_claim_path(&marker.rollout_path)
                        == canonical_rollout_claim_path(Path::new(&existing.output_path))
                    {
                        &own_claim
                    } else {
                        claimed_rollout_paths
                    };
                    matches!(
                        codex_tui_marker_rehydrate_decision(marker, claims, duplicate_marker_paths),
                        CodexTuiMarkerRehydrateDecision::Use { .. }
                    )
                });
                let (path, session_id) = marker
                    .map(|marker| (marker.rollout_path, marker.session_id))
                    .unwrap_or_else(|| {
                        (
                            PathBuf::from(&existing.output_path),
                            existing.session_id.clone(),
                        )
                    });
                let mut fresh = codex_tui_rehydrated_binding_from_rollout_path(
                    tmux_session_name,
                    &path,
                    session_id,
                )?;
                if canonical_rollout_claim_path(Path::new(&existing.output_path))
                    == canonical_rollout_claim_path(Path::new(&fresh.output_path))
                    && existing
                        .session_id
                        .as_deref()
                        .zip(fresh.session_id.as_deref())
                        .is_none_or(|(current, observed)| current.trim() == observed.trim())
                {
                    if existing
                        .relay_output_path
                        .as_ref()
                        .is_none_or(|path| Some(path) == fresh.relay_output_path.as_ref())
                    {
                        if crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(
                            tmux_session_name,
                        ) != Some(channel_id)
                        {
                            crate::services::tui_prompt_dedupe::register_tmux_channel_under_source_authority(
                                authority,
                                channel_id,
                            );
                        }
                        // #5755: None means no recovery in this pass. The sole
                        // caller continues, without success logging or fallback.
                        return None;
                    }
                    // A new relay namespace must not skip unread rollout bytes.
                    fresh.last_offset = existing.last_offset;
                }
                return Some((fresh, true));
            }
            rehydrated_codex_tui_binding_for_tmux_session(
                tmux_session_name,
                claimed_rollout_paths,
                reserved_rollout_paths,
                duplicate_marker_paths,
                allow_markerless_cwd_fallback,
            )
            .map(|fresh| (fresh, true))
        },
    )
}

#[cfg(all(unix, test))]
pub(crate) use codex_marker::run_codex_rehydrate_pass_for_tests;

/// One boot restore pass for a live pane, with no other pane's claims.
#[cfg(all(unix, test))]
pub(crate) fn rehydrate_codex_tui_binding_for_tests(
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    let none = HashSet::new();
    let claims = claimed_codex_tui_rollout_paths();
    rehydrate_codex_tui_binding_transaction(
        tmux_session_name,
        channel_id,
        &claims,
        &none,
        &none,
        false,
        || {},
    )
}

#[cfg(unix)]

fn tmux_session_is_codex_tui(tmux_session_name: &str) -> bool {
    if crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux_session_name)
        == Some(RuntimeHandoffKind::CodexTui)
    {
        return true;
    }
    crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
        .is_some_and(|binding| binding.runtime_kind == RuntimeHandoffKind::CodexTui)
}

#[cfg(unix)]
fn codex_rollout_session_id(rollout_path: &Path) -> Option<String> {
    let file = std::fs::File::open(rollout_path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut first_line).ok()?;
    let value: serde_json::Value = serde_json::from_str(first_line.trim()).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return None;
    }
    value
        .get("payload")
        .and_then(|payload| payload.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[cfg(unix)]
fn codex_tui_launch_modified_since(tmux_session_name: &str) -> std::time::SystemTime {
    crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "sh")
        .and_then(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
        })
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

#[cfg(unix)]
fn canonical_rollout_claim_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(unix)]
fn claimed_codex_tui_rollout_paths() -> HashSet<PathBuf> {
    crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(RuntimeHandoffKind::CodexTui)
        .into_iter()
        .filter_map(|(_, binding)| {
            let path = PathBuf::from(binding.output_path);
            path.exists().then(|| canonical_rollout_claim_path(&path))
        })
        .collect()
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct CodexTuiRehydratePlan {
    reserved_rollout_paths: HashSet<PathBuf>,
    duplicate_marker_paths: HashSet<PathBuf>,
    markerless_fallback_allowed_sessions: HashSet<String>,
}

#[cfg(unix)]
#[derive(Debug, Clone)]
struct CodexTuiRehydrateObservation {
    tmux_session_name: String,
    live_pane: bool,
    canonical_cwd: Option<PathBuf>,
    existing_binding_path: Option<PathBuf>,
    marker_path: Option<PathBuf>,
}

#[cfg(unix)]
fn codex_tui_rehydrate_observations(sessions: &[String]) -> Vec<CodexTuiRehydrateObservation> {
    let mut observations = Vec::new();
    for tmux_session_name in sessions {
        if !tmux_session_is_codex_tui(tmux_session_name) {
            continue;
        }
        let live_pane = codex_pass_pane_is_live(tmux_session_name);
        let canonical_cwd = live_pane
            .then(|| crate::services::platform::tmux::pane_current_path(tmux_session_name))
            .flatten()
            .map(PathBuf::from)
            .map(|cwd| std::fs::canonicalize(&cwd).unwrap_or(cwd));
        let marker_path =
            crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
                .and_then(|marker| {
                    marker
                        .rollout_path
                        .exists()
                        .then(|| canonical_rollout_claim_path(&marker.rollout_path))
                });
        observations.push(CodexTuiRehydrateObservation {
            tmux_session_name: tmux_session_name.clone(),
            live_pane,
            canonical_cwd,
            existing_binding_path: codex_tui_existing_valid_binding_path(tmux_session_name),
            marker_path,
        });
    }
    observations
}

#[cfg(unix)]
fn codex_tui_rehydrate_plan(sessions: &[String]) -> CodexTuiRehydratePlan {
    codex_tui_rehydrate_plan_from_observations(codex_tui_rehydrate_observations(sessions))
}

#[cfg(unix)]
fn codex_tui_rehydrate_plan_from_observations(
    observations: Vec<CodexTuiRehydrateObservation>,
) -> CodexTuiRehydratePlan {
    let mut tmux_sessions_by_marker_path: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for observation in &observations {
        let Some(marker_path) = observation.marker_path.as_ref() else {
            continue;
        };
        tmux_sessions_by_marker_path
            .entry(marker_path.clone())
            .or_default()
            .push(observation.tmux_session_name.clone());
    }

    let mut duplicate_marker_paths = HashSet::new();
    let mut reserved_rollout_paths: HashSet<PathBuf> = observations
        .iter()
        .filter_map(|observation| observation.existing_binding_path.clone())
        .collect();
    for (marker_path, tmux_sessions) in tmux_sessions_by_marker_path {
        reserved_rollout_paths.insert(marker_path.clone());
        if tmux_sessions.len() == 1 {
            continue;
        }
        duplicate_marker_paths.insert(marker_path.clone());
        tracing::debug!(
            marker_path = %marker_path.display(),
            sessions = ?tmux_sessions,
            "Codex TUI rollout marker path is shared by multiple sessions; treating as markerless ambiguity"
        );
    }

    let markerless_cwd_by_tmux = observations
        .iter()
        .filter_map(|observation| {
            codex_tui_observation_needs_markerless_fallback(observation, &duplicate_marker_paths)
                .then(|| {
                    observation
                        .canonical_cwd
                        .clone()
                        .map(|cwd| (observation.tmux_session_name.clone(), cwd))
                })
                .flatten()
        })
        .collect();
    let markerless_fallback_allowed_sessions =
        markerless_codex_tui_fallback_allowed_from_cwds(markerless_cwd_by_tmux);

    CodexTuiRehydratePlan {
        reserved_rollout_paths,
        duplicate_marker_paths,
        markerless_fallback_allowed_sessions,
    }
}

#[cfg(unix)]
fn codex_tui_existing_valid_binding_path(tmux_session_name: &str) -> Option<PathBuf> {
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)?;
    if binding.runtime_kind != RuntimeHandoffKind::CodexTui {
        return None;
    }
    let path = PathBuf::from(binding.output_path);
    path.exists().then(|| canonical_rollout_claim_path(&path))
}

#[cfg(unix)]
fn codex_tui_observation_needs_markerless_fallback(
    observation: &CodexTuiRehydrateObservation,
    duplicate_marker_paths: &HashSet<PathBuf>,
) -> bool {
    if !observation.live_pane || observation.existing_binding_path.is_some() {
        return false;
    }
    match observation.marker_path.as_ref() {
        Some(marker_path) => duplicate_marker_paths.contains(marker_path),
        None => true,
    }
}

#[cfg(unix)]
fn markerless_codex_tui_fallback_allowed_from_cwds(
    markerless_cwd_by_tmux: Vec<(String, PathBuf)>,
) -> HashSet<String> {
    let mut sessions_by_cwd: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for (tmux_session_name, cwd) in markerless_cwd_by_tmux {
        sessions_by_cwd
            .entry(cwd)
            .or_default()
            .push(tmux_session_name);
    }
    let mut allowed = HashSet::new();
    for (cwd, sessions) in sessions_by_cwd {
        if sessions.len() == 1 {
            allowed.insert(sessions[0].clone());
            continue;
        }
        tracing::debug!(
            cwd = %cwd.display(),
            session_count = sessions.len(),
            sessions = ?sessions,
            "skipping markerless Codex TUI rollout rehydrate fallback for ambiguous same-cwd sessions"
        );
    }
    allowed
}

#[cfg(unix)]
pub(in crate::services::discord) fn codex_tui_rehydrated_binding_from_rollout_path(
    tmux_session_name: &str,
    rollout_path: &Path,
    session_id: Option<String>,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    if !rollout_path.exists()
        || crate::services::codex_tui::rollout_index::rollout_is_subagent(rollout_path)
    {
        return None;
    }
    let start_offset = std::fs::metadata(rollout_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let relay_output_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let relay_last_offset = std::fs::metadata(&relay_output_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    Some(crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::CodexTui,
        output_path: rollout_path.display().to_string(),
        relay_output_path: Some(relay_output_path),
        input_fifo_path: None,
        session_id: session_id.or_else(|| codex_rollout_session_id(rollout_path)),
        last_offset: start_offset,
        relay_last_offset: Some(relay_last_offset),
    })
}

#[cfg(unix)]
pub(in crate::services::discord) fn rehydrated_codex_tui_binding_for_tmux_session(
    tmux_session_name: &str,
    claimed_rollout_paths: &HashSet<PathBuf>,
    reserved_rollout_paths: &HashSet<PathBuf>,
    duplicate_marker_paths: &HashSet<PathBuf>,
    allow_markerless_cwd_fallback: bool,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    if let Some(marker) =
        crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
    {
        match codex_tui_marker_rehydrate_decision(
            &marker,
            claimed_rollout_paths,
            duplicate_marker_paths,
        ) {
            CodexTuiMarkerRehydrateDecision::Use {
                rollout_path,
                session_id,
            } => {
                return codex_tui_rehydrated_binding_from_rollout_path(
                    tmux_session_name,
                    &rollout_path,
                    session_id,
                );
            }
            CodexTuiMarkerRehydrateDecision::TryFallback => {}
        }
        tracing::debug!(
            tmux_session_name,
            rollout_path = %marker.rollout_path.display(),
            "skipping Codex TUI rehydrate from child, stale, duplicate, or already-claimed rollout marker; trying markerless fallback if permitted"
        );
    }
    if !allow_markerless_cwd_fallback {
        tracing::debug!(
            tmux_session_name,
            "skipping markerless Codex TUI rollout rehydrate fallback because cwd is ambiguous"
        );
        return None;
    }
    let cwd = crate::services::platform::tmux::pane_current_path(tmux_session_name)?;
    let sessions_dir = crate::services::codex_tui::rollout_tail::default_codex_sessions_dir()?;
    let mut excluded_rollout_paths = reserved_rollout_paths.clone();
    excluded_rollout_paths.extend(claimed_rollout_paths.iter().cloned());
    let rollout_path =
        crate::services::codex_tui::rollout_tail::latest_unclaimed_rollout_for_cwd_since(
            Path::new(&cwd),
            codex_tui_launch_modified_since(tmux_session_name),
            &sessions_dir,
            &excluded_rollout_paths,
        )?;
    codex_tui_rehydrated_binding_from_rollout_path(tmux_session_name, &rollout_path, None)
}

#[cfg(unix)]
fn rollout_path_is_claimed_for_other_session(
    path: &Path,
    claimed_rollout_paths: &HashSet<PathBuf>,
) -> bool {
    claimed_rollout_paths.contains(path)
        || claimed_rollout_paths.contains(&canonical_rollout_claim_path(path))
}

#[cfg(all(unix, test))]
mod idempotency_tests;

#[cfg(all(unix, test))]
mod child_binding_tests;

#[cfg(all(unix, test))]
mod tests {
    use super::*;

    #[test]
    fn codex_rollout_session_id_reads_first_session_meta_line() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rollout_path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"019f0111-fc32\"}}\n",
                "{\"type\":\"event\",\"payload\":{}}\n"
            ),
        )
        .expect("write rollout");

        assert_eq!(
            codex_rollout_session_id(&rollout_path),
            Some("019f0111-fc32".to_string())
        );
    }

    #[test]
    fn codex_rollout_session_id_rejects_non_meta_first_line() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rollout_path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            concat!(
                "{\"type\":\"event\",\"payload\":{}}\n",
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"late\"}}\n"
            ),
        )
        .expect("write rollout");

        assert_eq!(codex_rollout_session_id(&rollout_path), None);
    }

    #[test]
    fn markerless_codex_tui_fallback_rejects_ambiguous_same_cwd_sessions() {
        let cwd = PathBuf::from("/tmp/agentdesk-shared-cwd");
        let allowed = markerless_codex_tui_fallback_allowed_from_cwds(vec![
            ("AgentDesk-codex-a".to_string(), cwd.clone()),
            ("AgentDesk-codex-b".to_string(), cwd),
        ]);

        assert!(
            allowed.is_empty(),
            "markerless same-cwd sessions have no stable rollout identity; rehydrate must skip rather than cross-wire channels"
        );
    }

    #[test]
    fn markerless_codex_tui_fallback_allows_unambiguous_cwd_sessions() {
        let allowed = markerless_codex_tui_fallback_allowed_from_cwds(vec![
            (
                "AgentDesk-codex-a".to_string(),
                PathBuf::from("/tmp/agentdesk-cwd-a"),
            ),
            (
                "AgentDesk-codex-b".to_string(),
                PathBuf::from("/tmp/agentdesk-cwd-b"),
            ),
        ]);

        assert_eq!(allowed.len(), 2);
        assert!(allowed.contains("AgentDesk-codex-a"));
        assert!(allowed.contains("AgentDesk-codex-b"));
    }

    fn codex_tui_observation(
        tmux_session_name: &str,
        cwd: Option<&str>,
        existing_binding_path: Option<&str>,
        marker_path: Option<&str>,
    ) -> CodexTuiRehydrateObservation {
        CodexTuiRehydrateObservation {
            tmux_session_name: tmux_session_name.to_string(),
            live_pane: cwd.is_some(),
            canonical_cwd: cwd.map(PathBuf::from),
            existing_binding_path: existing_binding_path.map(PathBuf::from),
            marker_path: marker_path.map(PathBuf::from),
        }
    }

    #[test]
    fn codex_tui_marker_rehydrate_decision_uses_valid_marker() {
        let dir = tempfile::tempdir().expect("temp dir");
        let rollout_path = dir.path().join("rollout.jsonl");
        std::fs::write(
            &rollout_path,
            "{\"type\":\"session_meta\",\"payload\":{}}\n",
        )
        .expect("write rollout");
        let marker = crate::services::codex_tui::session::CodexTuiRolloutMarker {
            rollout_path: rollout_path.clone(),
            session_id: Some("sess-1".to_string()),
            rollout_start_offset: None,
        };

        assert_eq!(
            codex_tui_marker_rehydrate_decision(&marker, &HashSet::new(), &HashSet::new()),
            CodexTuiMarkerRehydrateDecision::Use {
                rollout_path,
                session_id: Some("sess-1".to_string()),
            }
        );
    }

    #[test]
    fn codex_tui_marker_rehydrate_decision_falls_back_for_unusable_marker() {
        let dir = tempfile::tempdir().expect("temp dir");
        let existing_rollout_path = dir.path().join("existing-rollout.jsonl");
        std::fs::write(
            &existing_rollout_path,
            "{\"type\":\"session_meta\",\"payload\":{}}\n",
        )
        .expect("write rollout");
        let stale_marker = crate::services::codex_tui::session::CodexTuiRolloutMarker {
            rollout_path: dir.path().join("deleted-rollout.jsonl"),
            session_id: Some("stale".to_string()),
            rollout_start_offset: None,
        };
        let duplicate_marker = crate::services::codex_tui::session::CodexTuiRolloutMarker {
            rollout_path: existing_rollout_path.clone(),
            session_id: Some("duplicate".to_string()),
            rollout_start_offset: None,
        };
        let claimed_marker = crate::services::codex_tui::session::CodexTuiRolloutMarker {
            rollout_path: existing_rollout_path.clone(),
            session_id: Some("claimed".to_string()),
            rollout_start_offset: None,
        };
        let mut duplicate_marker_paths = HashSet::new();
        duplicate_marker_paths.insert(canonical_rollout_claim_path(&existing_rollout_path));
        let mut claimed_rollout_paths = HashSet::new();
        claimed_rollout_paths.insert(canonical_rollout_claim_path(&existing_rollout_path));

        assert_eq!(
            codex_tui_marker_rehydrate_decision(&stale_marker, &HashSet::new(), &HashSet::new(),),
            CodexTuiMarkerRehydrateDecision::TryFallback
        );
        assert_eq!(
            codex_tui_marker_rehydrate_decision(
                &duplicate_marker,
                &HashSet::new(),
                &duplicate_marker_paths,
            ),
            CodexTuiMarkerRehydrateDecision::TryFallback
        );
        assert_eq!(
            codex_tui_marker_rehydrate_decision(
                &claimed_marker,
                &claimed_rollout_paths,
                &HashSet::new(),
            ),
            CodexTuiMarkerRehydrateDecision::TryFallback
        );
    }

    #[test]
    fn codex_tui_rehydrate_plan_reserves_marker_paths_before_markerless_fallback() {
        let marker_path = "/tmp/agentdesk-marker-backed-rollout.jsonl";
        let plan = codex_tui_rehydrate_plan_from_observations(vec![
            codex_tui_observation("AgentDesk-codex-markerless", Some("/repo"), None, None),
            codex_tui_observation(
                "AgentDesk-codex-marker-backed",
                Some("/repo"),
                None,
                Some(marker_path),
            ),
        ]);

        assert!(
            plan.reserved_rollout_paths
                .contains(&PathBuf::from(marker_path)),
            "marker-backed rollout must be reserved before markerless cwd fallback runs"
        );
        assert!(
            plan.markerless_fallback_allowed_sessions
                .contains("AgentDesk-codex-markerless"),
            "the markerless sibling may fallback, but only after excluding the reserved marker path"
        );
    }

    #[test]
    fn codex_tui_rehydrate_plan_counts_stale_marker_sessions_as_markerless_ambiguity() {
        let plan = codex_tui_rehydrate_plan_from_observations(vec![
            codex_tui_observation("AgentDesk-codex-markerless", Some("/repo"), None, None),
            // A stale marker has no usable rollout path, so it is represented as
            // live but without `marker_path` and must count against cwd fallback.
            codex_tui_observation("AgentDesk-codex-stale-marker", Some("/repo"), None, None),
        ]);

        assert!(
            plan.markerless_fallback_allowed_sessions.is_empty(),
            "two live sessions with no usable rollout identity in the same cwd must skip fallback"
        );
    }

    /// The rehydration pass's dead-pane eviction against the rows stored for each session.
    mod host_defer {
        use super::*;
        use crate::db::dispatched_sessions::hosted_execution;
        use crate::db::dispatched_sessions::hosted_execution::HostedState;
        use crate::db::dispatched_sessions::hosted_execution::tests::{
            future_schema, owner, record, wire,
        };
        use crate::services::discord::host_defer_gate::tests::{
            Case, ScriptedTmux, postgres, with_second_bot,
        };
        use crate::services::discord::host_teardown_gate::test_support::Stored;

        fn binding(
            kind: RuntimeHandoffKind,
        ) -> crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: kind,
                output_path: "/runtime/p4c1-rehydrate.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            }
        }

        // A dead pane's relay mirror and restored owner are dropped only for a session the host
        // guard admits, in the unlisted sweep and the listed-pane pass of both providers.
        #[tokio::test(flavor = "multi_thread")]
        async fn a_dead_pane_mirror_is_evicted_only_for_what_the_host_guard_admits_pg() {
            let _root = crate::config::TestRuntimeRootGuard::new();
            let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            crate::services::tui_prompt_dedupe::reset_state_for_tests();
            let tmux = ScriptedTmux::install();
            let (db, pool) = postgres().await;
            let bot_b = "p4c1-bot-b";
            let (shared, _registry) = with_second_bot(&pool, bot_b).await;
            let providers = [
                (ProviderKind::Claude, RuntimeHandoffKind::ClaudeTui),
                (ProviderKind::Codex, RuntimeHandoffKind::CodexTui),
            ];
            let mut sessions = Vec::new();
            for (p, (provider, kind)) in providers.iter().enumerate() {
                for listed in [false, true] {
                    for (n, case) in Case::ALL.into_iter().enumerate() {
                        let at = (p * 2 + usize::from(listed)) * 10 + n;
                        let channel = 1_479_671_301_387_065_000 + at as u64;
                        let name = format!("AgentDesk-{}-p4c1-evict-{at}", provider.as_str());
                        let key =
                            crate::services::discord::adk_session::build_namespaced_session_key(
                                &shared.token_hash,
                                provider,
                                &name,
                            );
                        case.seed(&pool, &key, &name, channel).await;
                        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                            &name,
                            binding(*kind),
                        );
                        crate::services::tui_prompt_dedupe::register_tmux_channel(&name, channel);
                        let owner = ChannelId::new(channel);
                        shared
                            .tmux_watchers
                            .restore_owner_channel_for_tmux_session(&name, owner);
                        sessions.push((name, key, listed, case));
                    }
                }
            }
            // A second bot's rows sit under its own hash, which only the registry names.
            let mut second = Vec::new();
            for (n, stored) in [Stored::Legacy, Stored::Hosted, Stored::Future]
                .into_iter()
                .enumerate()
            {
                let channel = 1_479_671_301_387_065_800 + n as u64;
                let name = format!("AgentDesk-claude-p4c1-bot-b-{n}");
                let claude = ProviderKind::Claude;
                let key = crate::services::discord::adk_session::build_namespaced_session_key(
                    bot_b, &claude, &name,
                );
                let mut row_owner = owner(&channel.to_string());
                row_owner.discord_token_hash = bot_b.to_string();
                let raw = match stored {
                    Stored::Hosted => Some(wire(&record(&row_owner, "n1", HostedState::Bound))),
                    Stored::Future => Some(future_schema(&row_owner)),
                    _ => None,
                };
                let seed = crate::services::discord::host_key_derivation::tests::seed_row;
                seed(&pool, "claude", Some(bot_b), &key, channel, raw).await;
                let tui = binding(RuntimeHandoffKind::ClaudeTui);
                crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&name, tui);
                crate::services::tui_prompt_dedupe::register_tmux_channel(&name, channel);
                second.push((name, stored == Stored::Legacy));
            }
            let unread = "AgentDesk-claude-p4c1-evict-unread";
            let key = crate::services::discord::host_teardown_gate::test_support::channel_key;
            let legacy = Stored::Legacy;
            let unread_channel = 1_479_671_301_387_065_900;
            Case::Stored(legacy)
                .seed(&pool, &key(&shared, unread), unread, unread_channel)
                .await;
            let listed: Vec<&str> = sessions
                .iter()
                .filter(|(_, _, listed, _)| *listed)
                .map(|(name, ..)| name.as_str())
                .collect();
            let rows = |keys: Vec<String>| {
                let pool = pool.clone();
                async move {
                    let mut rows = Vec::new();
                    for key in keys {
                        let key = hosted_execution::HostedLookupKey::SessionKey(&key);
                        let row = hosted_execution::load_hosted_execution_pg(&pool, key).await;
                        rows.push(format!("{row:?}"));
                    }
                    rows
                }
            };
            let keys = || sessions.iter().map(|(_, key, ..)| key.clone()).collect();
            let rows_before = rows(keys()).await;
            tmux.list(&listed);
            let pass = shared.clone();
            tokio::task::spawn_blocking(move || {
                rehydrate_existing_claude_tui_bindings(&pass);
                rehydrate_existing_codex_tui_bindings(&pass);
            })
            .await
            .expect("rehydrate pass");
            assert_eq!(rows(keys()).await, rows_before, "eviction is memory only");
            for (name, _, listed, case) in &sessions {
                let mirrored =
                    crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(name)
                        .is_some();
                let owned = shared
                    .tmux_watchers
                    .owner_channel_for_tmux_session(name)
                    .is_some();
                let label = format!("{name} listed={listed} {case:?}");
                assert_eq!(mirrored, !case.admitted(), "{label}: mirror");
                assert_eq!(owned, !case.admitted(), "{label}: restored owner");
            }

            for (name, admitted) in &second {
                let mirrored =
                    crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(name)
                        .is_some();
                assert_eq!(mirrored, !admitted, "{name}: second bot's mirror");
            }

            let claude_tui = binding(RuntimeHandoffKind::ClaudeTui);
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(unread, claude_tui);
            crate::services::tui_prompt_dedupe::register_tmux_channel(unread, unread_channel);
            pool.close().await;
            let pass = shared.clone();
            tokio::task::spawn_blocking(move || rehydrate_existing_claude_tui_bindings(&pass))
                .await
                .expect("rehydrate pass");
            let kept = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(unread);
            assert!(kept.is_some(), "a failed row read is not a legacy answer");
            db.drop().await;
        }
    }
}
