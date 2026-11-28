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
pub(in crate::services::discord) mod codex_marker;
#[cfg(unix)]
use codex_marker::{CodexTuiMarkerRehydrateDecision, codex_tui_marker_rehydrate_decision};
#[cfg(unix)]
use codex_marker::{codex_rollout_session_id, codex_tui_launch_modified_since};

#[cfg(any(not(test), not(unix)))]
use super::resolve_rehydrated_claude_tmux_channel_id as claude_channel;
#[cfg(any(not(test), not(unix)))]
use crate::services::tmux_diagnostics::tmux_session_has_live_pane as claude_pane_live;
#[cfg(any(not(test), not(unix)))]
fn claude_session_names() -> Result<Vec<String>, String> {
    crate::services::session_host::TmuxHost.list_session_names()
}
#[cfg(all(test, unix))]
use claude_pass_tests::{claude_channel, claude_pane_live, claude_session_names};
#[cfg(test)]
#[path = "../../claude_tui/hook_server/rehydration_ingress_tests.rs"]
pub(super) mod claude_pass_tests;

#[cfg(unix)]
pub(super) fn claude_tui_session_is_dead_orphaned(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    epoch: &TmuxMirrorEpoch,
) -> Option<bool> {
    tmux_pane_is_dead_orphaned(shared, tmux_session_name, epoch, claude_pane_live)
}

#[cfg(unix)]
fn tmux_pane_is_dead_orphaned(
    shared: &SharedData,
    name: &str,
    epoch: &TmuxMirrorEpoch,
    pane_live: impl Fn(&str) -> bool,
) -> Option<bool> {
    if shared.tmux_watchers.has_live_watcher_handle(name) {
        return Some(false);
    }
    pane_is_confirmed_dead_orphaned(
        || epoch.probe(name, || pane_live(name)),
        || {
            epoch.probe(name, || {
                crate::services::tmux_diagnostics::tmux_session_pane_liveness(name)
            })
        },
        DEAD_ORPHANED_PANE_PROBE_SAMPLES,
        Some(DEAD_ORPHANED_PANE_PROBE_DELAY),
    )
}

#[cfg(all(test, unix))]
thread_local! {
    pub(super) static AFTER_DEAD_PANE_SAMPLE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
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
    mut has_live_pane: impl FnMut() -> Option<bool>,
    session_liveness: impl FnOnce() -> Option<crate::services::platform::tmux::PaneLiveness>,
    samples: usize,
    inter_probe_delay: Option<Duration>,
) -> Option<bool> {
    let samples = samples.max(1);
    for sample in 0..samples {
        if has_live_pane()? {
            // Any live observation across the window means the session is alive
            // (or recovered from a flake): never evict.
            return Some(false);
        }
        #[cfg(test)]
        if let Some(after_sample) = AFTER_DEAD_PANE_SAMPLE.with_borrow_mut(Option::take) {
            after_sample();
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
    Some(matches!(
        session_liveness()?,
        crate::services::platform::tmux::PaneLiveness::DeadOrAbsent
    ))
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
pub(super) fn evict_dead_orphaned_claude_tui_mirrors(shared: &Arc<SharedData>) -> MirrorHosts {
    evict_dead_orphaned_tui_mirrors(
        shared,
        &ProviderKind::Claude,
        RuntimeHandoffKind::ClaudeTui,
        claude_tui_session_is_dead_orphaned,
    )
}

#[cfg(unix)]
fn codex_tui_session_is_dead_orphaned(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    epoch: &TmuxMirrorEpoch,
) -> Option<bool> {
    tmux_pane_is_dead_orphaned(shared, tmux_session_name, epoch, codex_pass_pane_is_live)
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
    crate::services::session_host::TmuxHost.list_session_names()
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
fn evict_dead_orphaned_codex_tui_mirrors(shared: &Arc<SharedData>) -> MirrorHosts {
    evict_dead_orphaned_tui_mirrors(
        shared,
        &ProviderKind::Codex,
        RuntimeHandoffKind::CodexTui,
        codex_tui_session_is_dead_orphaned,
    )
}

#[cfg(unix)]
fn evict_dead_orphaned_tui_mirrors(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    kind: RuntimeHandoffKind,
    dead_orphaned: impl Fn(&Arc<SharedData>, &str, &TmuxMirrorEpoch) -> Option<bool>,
) -> MirrorHosts {
    let mut hosts = HashMap::new();
    for (name, binding) in crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(kind) {
        let channel = crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(&name);
        let restored = shared.tmux_watchers.owner_channel_for_tmux_session(&name);
        let mut host = host_admits(shared, provider, &name, None);
        match &host {
            MirrorHost::Herdr(nonce) => {
                evict_herdr_mirror(shared, &name, &binding, nonce.as_deref())
            }
            MirrorHost::Tmux(epoch) => {
                let outcome = match dead_orphaned(shared, &name, epoch) {
                    Some(true) => epoch.evict_confirmed_orphan(
                        shared,
                        provider,
                        &name,
                        Some(&binding),
                        channel,
                        restored,
                    ),
                    Some(false) => Some(()),
                    None => None,
                };
                if outcome.is_none() {
                    host = MirrorHost::Deferred;
                }
            }
            MirrorHost::Deferred => {}
        }
        hosts.insert(name, host);
    }
    hosts
}

#[cfg(unix)]
fn evict_herdr_mirror(
    shared: &SharedData,
    tmux_session_name: &str,
    binding: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    herdr_nonce: Option<&str>,
) {
    crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
        if crate::services::tui_prompt_dedupe::evict_previous_tmux_mirror_under_source_authority(
            authority,
            binding,
            herdr_nonce,
        ) {
            shared
                .tmux_watchers
                .clear_restored_owner_for_tmux_session(tmux_session_name);
            tracing::debug!(
                tmux_session_name,
                "evicted stale tmux mirror for Herdr session"
            );
        }
    });
}

#[cfg(unix)]
type MirrorHosts = HashMap<String, MirrorHost>;

#[cfg(unix)]
type MirrorHost = super::super::host_defer_gate::MirrorHost;

#[cfg(unix)]
pub(super) type TmuxMirrorEpoch = super::super::host_defer_gate::TmuxMirrorEpoch;

#[cfg(unix)]
pub(super) fn rehydrate_existing_claude_tui_bindings(shared: &Arc<SharedData>) {
    // Deferred hook adoptions go first so this pass sees their binding.
    crate::services::claude_tui::hook_server::retry_deferred_claude_adoptions();
    // #3105 (codex P1 sub-case B): tombstone stale mirrors for dead/orphaned
    // sessions BEFORE anything else, so the per-poll drift/skip WARN spam stops
    // even when the session is not present in `list_session_names()` at all.
    let hosts = evict_dead_orphaned_claude_tui_mirrors(shared);

    match claude_session_names() {
        Ok(sessions) => {
            super::super::host_defer_gate::retain_mirror_lookup_warnings(
                &ProviderKind::Claude,
                |name| hosts.contains_key(name) || sessions.iter().any(|listed| listed == name),
            );
            for tmux_session_name in sessions {
                let _ = rehydrate_claude_tui_pane_with_hosts(shared, &tmux_session_name, &hosts);
            }
            crate::services::claude_tui::hook_server::mark_boot_discovery_complete();
        }
        Err(error) => {
            tracing::debug!(error = %error, "Claude TUI binding rehydrate skipped; tmux sessions unavailable");
        }
    }
    // Herdr panes are not tmux sessions; their rows name them. No local endpoint reads nothing.
    super::super::recovery_engine::herdr_reader::reconnect_restarted_herdr_with_shared(
        shared,
        &ProviderKind::Claude,
    );
}

/// One pane of the rehydrate pass; a durable Pending is restored before the binding judgment.
#[cfg(all(test, unix))]
fn rehydrate_claude_tui_pane(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    rehydrate_claude_tui_pane_with_hosts(shared, tmux_session_name, &HashMap::new())
}

#[cfg(unix)]
fn rehydrate_claude_tui_pane_with_hosts(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    hosts: &MirrorHosts,
) -> Option<tokio::task::JoinHandle<()>> {
    let before =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name);
    rehydrate_claude_tui_pane_inner(shared, tmux_session_name, hosts);
    if crate::engine::ops::turn_ops::health_registry().is_some() {
        return None;
    }
    wake_attached_observer(
        shared,
        &ProviderKind::Claude,
        tmux_session_name,
        before.as_ref(),
    )
}

#[cfg(unix)]
fn wake_attached_observer(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux: &str,
    before: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> Option<tokio::task::JoinHandle<()>> {
    let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux)?;
    let source = |b: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding| {
        (b.runtime_kind, b.session_id.clone(), b.output_path.clone())
    };
    if before.map(source) == Some(source(&binding)) {
        return None;
    }
    let channel = crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux)?;
    wake_observed_source(
        shared,
        provider,
        tmux.to_owned(),
        channel,
        binding,
        #[cfg(test)]
        crate::services::tui_prompt_dedupe::binding_events::test_root(),
    )
}

/// A newly published source wakes through its channel's existing runtime and owner drive.
#[cfg(unix)]
pub(crate) fn observer_binding_published(
    tmux: String,
    published: crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) {
    #[cfg(test)]
    if let Some(published) = OBSERVER_PUBLICATION_FOR_TESTS
        .lock()
        .unwrap()
        .get(&tmux)
        .cloned()
    {
        published();
    }
    #[cfg(test)]
    let binding_root = crate::services::tui_prompt_dedupe::binding_events::test_root();
    let Some(registry) = crate::engine::ops::turn_ops::health_registry() else {
        return;
    };
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let provider = match published.runtime_kind {
        RuntimeHandoffKind::ClaudeTui => ProviderKind::Claude,
        RuntimeHandoffKind::CodexTui => ProviderKind::Codex,
        _ => return,
    };
    runtime.spawn(async move {
        // Publication keeps source authority until its channel mapping is installed too.
        let attached = tokio::task::spawn_blocking(move || {
            crate::services::tmux_common::with_tmux_source_authority(&tmux, |authority| {
                let current = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session_under_source_authority(authority)?;
                if (current.runtime_kind, &current.session_id, &current.output_path)
                    != (published.runtime_kind, &published.session_id, &published.output_path)
                {
                    return None;
                }
                let channel = crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(&tmux)?;
                Some((tmux.clone(), channel, current))
            })
        }).await;
        let Ok(Some((tmux, channel, binding))) = attached else {
            return;
        };
        let shared = registry.shared_for_provider_on_channel(&provider, ChannelId::new(channel)).await;
        if let Some(shared) = shared
            && let Some(wake) = wake_observed_source(&shared, &provider, tmux, channel, binding,
                #[cfg(test)] binding_root,
            )
        {
            let _ = wake.await;
        }
    });
}

#[cfg(unix)]
fn wake_observed_source(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux: String,
    channel: u64,
    binding: crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    #[cfg(test)] binding_root: Option<std::path::PathBuf>,
) -> Option<tokio::task::JoinHandle<()>> {
    if binding.session_id.as_deref().is_none_or(str::is_empty)
        || std::fs::metadata(&binding.output_path).is_err()
    {
        return None;
    }
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let (shared, provider) = (shared.clone(), provider.clone());
    Some(runtime.spawn(async move {
        let channel_id = ChannelId::new(channel);
        let mailbox = shared.mailbox(channel_id);
        let finished = shared.mailboxes.turn_finished(channel_id);
        let pending = mailbox
            .v2_request(|slot| Ok(slot.as_ref().is_some_and(|owner| owner.front().is_some())));
        if pending.await != Ok(true) {
            return;
        }
        // An idle refusal answers its drive before its bridge releases the turn token.
        if mailbox.has_active_turn().await == Ok(true) {
            #[cfg(test)]
            if let Some(waiting) = OBSERVER_WAITING_FOR_TESTS
                .lock()
                .unwrap()
                .get(&channel)
                .cloned()
            {
                waiting();
            }
            finished.wait().await;
        }
        let pending = mailbox
            .v2_request(|slot| Ok(slot.as_ref().is_some_and(|owner| owner.front().is_some())));
        if pending.await != Ok(true) {
            return;
        }
        let published = super::super::turn_presence::supervisor::observe_source_busy(
            &shared,
            &provider,
            channel,
            &tmux,
            &binding,
            #[cfg(test)]
            binding_root,
        )
        .await;
        if published.unwrap_or_else(|| {
            crate::services::tui_turn_state::observe_provider_jsonl_turn_state(
                &provider,
                Path::new(&binding.output_path),
            )
            .is_busy()
        }) {
            #[cfg(test)]
            observer_busy_seen_for_tests(channel);
            return;
        }
        let pending = mailbox
            .v2_request(|slot| Ok(slot.as_ref().is_some_and(|owner| owner.front().is_some())));
        if pending.await != Ok(true) {
            return;
        }
        #[cfg(test)]
        let kickoff = OBSERVER_KICKOFF_FOR_TESTS
            .lock()
            .unwrap()
            .get(&channel)
            .cloned();
        #[cfg(test)]
        if let Some(kickoff) = kickoff {
            kickoff();
            return;
        }
        // The existing drive coalesces a source attaching during its refused handoff.
        super::super::schedule_deferred_idle_queue_kickoff_immediate(
            shared,
            provider,
            channel_id,
            "idle_observer_attached",
        );
    }))
}

#[cfg(all(test, unix))]
type ObserverKickoff = Arc<dyn Fn() + Send + Sync>;
#[cfg(all(test, unix))]
pub(in crate::services::discord) static OBSERVER_KICKOFF_FOR_TESTS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<u64, ObserverKickoff>>,
> = std::sync::LazyLock::new(Default::default);
#[cfg(all(test, unix))]
pub(in crate::services::discord) static OBSERVER_WAITING_FOR_TESTS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<u64, ObserverKickoff>>,
> = std::sync::LazyLock::new(Default::default);
#[cfg(all(test, unix))]
pub(in crate::services::discord) static OBSERVER_BUSY_FOR_TESTS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<u64, ObserverKickoff>>,
> = std::sync::LazyLock::new(Default::default);
#[cfg(all(test, unix))]
fn observer_busy_seen_for_tests(channel: u64) {
    let seen = OBSERVER_BUSY_FOR_TESTS
        .lock()
        .unwrap()
        .get(&channel)
        .cloned();
    if let Some(seen) = seen {
        seen();
    }
}

#[cfg(all(test, unix))]
pub(in crate::services::discord) static OBSERVER_PUBLICATION_FOR_TESTS: std::sync::LazyLock<
    std::sync::Mutex<HashMap<String, ObserverKickoff>>,
> = std::sync::LazyLock::new(Default::default);

#[cfg(unix)]
fn rehydrate_claude_tui_pane_inner(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    hosts: &MirrorHosts,
) {
    // #3018: dedupe lookup here is a diagnostic/mirror rehydration hint only
    // (subordinate to the freshly resolved channel below), never a routing
    // authority — the authoritative resolver is owner_channel_for_tmux_session.
    let existing_channel =
        crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux_session_name);
    // #3105: prefer the settings-derived (authoritative) channel; only fall
    // back to the dedupe mirror's last-seen channel for the dedupe binding
    // refresh below. The mirror's value must NOT be promoted into the
    // authoritative registry — see the repair gate below.
    let authoritative_channel = claude_channel(tmux_session_name);
    let channel_id = match authoritative_channel.or(existing_channel) {
        Some(channel_id) if !names_another_host(tmux_session_name) => channel_id,
        _ => return,
    };
    let host = hosts.get(tmux_session_name).cloned().unwrap_or_else(|| {
        host_admits(
            shared,
            &ProviderKind::Claude,
            tmux_session_name,
            Some(channel_id),
        )
    });
    let MirrorHost::Tmux(epoch) = host else {
        return;
    };
    let existing_binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name);
    let restored = shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session_name);
    match epoch.probe(tmux_session_name, || claude_pane_live(tmux_session_name)) {
        None => return,
        Some(false) => {
            if claude_tui_session_is_dead_orphaned(shared, tmux_session_name, &epoch) == Some(true)
            {
                epoch.evict_confirmed_orphan(
                    shared,
                    &ProviderKind::Claude,
                    tmux_session_name,
                    existing_binding.as_ref(),
                    existing_channel,
                    restored,
                );
            }
            return;
        }
        Some(true) => {}
    }
    let (fresh_binding, launch_context) = claude_launch_observation(tmux_session_name);
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
    // Read after the restore so its own registration is judged; a later hook move gives it up.
    let existing_binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name);
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

/// One Codex discovery pass: Herdr reconnect, then owner repair and binding of live tmux panes.
#[cfg(unix)]
pub(super) fn rehydrate_existing_codex_tui_bindings(shared: &Arc<SharedData>) {
    let mut hosts = evict_dead_orphaned_codex_tui_mirrors(shared);
    // Codex Herdr rows, read whether or not tmux answers; off Herdr this reads nothing.
    super::super::recovery_engine::herdr_reader::reconnect_restarted_herdr_with_shared(
        shared,
        &ProviderKind::Codex,
    );
    let mut sessions = match codex_pass_tmux_session_names() {
        Ok(sessions) => sessions,
        Err(error) => {
            tracing::debug!(error = %error, "Codex TUI binding rehydrate skipped; tmux sessions unavailable");
            return;
        }
    };
    for name in &sessions {
        if tmux_session_is_codex_tui(name) {
            hosts.entry(name.clone()).or_insert_with(|| {
                let channel = resolve_rehydrated_tmux_channel_id(&ProviderKind::Codex, name)
                    .or_else(|| {
                        crate::services::tui_prompt_dedupe::codex_verified_discovered_channel(name)
                    });
                host_admits(shared, &ProviderKind::Codex, name, channel)
            });
        }
    }
    super::super::host_defer_gate::retain_mirror_lookup_warnings(&ProviderKind::Codex, |name| {
        hosts.contains_key(name)
    });
    sessions.sort_by(|left, right| {
        codex_tui_launch_modified_since(right)
            .cmp(&codex_tui_launch_modified_since(left))
            .then_with(|| left.cmp(right))
    });

    let rehydrate_plan = codex_tui_rehydrate_plan(&sessions, &mut hosts);
    let mut claimed_rollout_paths = claimed_codex_tui_rollout_paths();

    for tmux_session_name in sessions {
        let Some(MirrorHost::Tmux(epoch)) = hosts.get(&tmux_session_name) else {
            continue;
        };
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
        let mirror_channel =
            crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(&tmux_session_name);
        let Some(channel_id) = authoritative_channel.or(mirror_channel) else {
            continue;
        };
        let before = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
            &tmux_session_name,
        );
        let restored = shared
            .tmux_watchers
            .owner_channel_for_tmux_session(&tmux_session_name);
        match epoch.probe(&tmux_session_name, || {
            codex_pass_pane_is_live(&tmux_session_name)
        }) {
            None => continue,
            Some(false) => {
                if codex_tui_session_is_dead_orphaned(shared, &tmux_session_name, epoch)
                    == Some(true)
                {
                    epoch.evict_confirmed_orphan(
                        shared,
                        &ProviderKind::Codex,
                        &tmux_session_name,
                        before.as_ref(),
                        mirror_channel,
                        restored,
                    );
                }
                continue;
            }
            Some(true) => {}
        }

        if let Some(authoritative_channel) = authoritative_channel {
            let Some(repaired) = codex_marker::restore_codex_owner_channel(
                shared,
                &tmux_session_name,
                authoritative_channel,
                epoch,
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
            (&tmux_session_name, epoch),
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
        if crate::engine::ops::turn_ops::health_registry().is_none() {
            let _ = wake_attached_observer(
                shared,
                &ProviderKind::Codex,
                &tmux_session_name,
                before.as_ref(),
            );
        }
        claimed_rollout_paths.insert(canonical_rollout_claim_path(Path::new(&fresh.output_path)));
    }
}

#[cfg(unix)]
fn rehydrate_codex_tui_binding_transaction(
    pane: (&str, &TmuxMirrorEpoch),
    channel_id: u64,
    claimed_rollout_paths: &HashSet<PathBuf>,
    reserved_rollout_paths: &HashSet<PathBuf>,
    duplicate_marker_paths: &HashSet<PathBuf>,
    allow_markerless_cwd_fallback: bool,
    observe_before_register: impl FnOnce(),
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    let (tmux_session_name, epoch) = pane;
    // Restore durable claims before discovery asks the generic publication gate.
    let recovered = crate::services::tmux_common::with_tmux_source_authority(
        tmux_session_name,
        |authority| {
            epoch.is_current(authority)
                && crate::services::tui_prompt_dedupe::recover_discovered_codex_binding_under_source_authority(
                    authority, channel_id,
                )
        },
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
            if !epoch.is_current(authority) {
                return None;
            }
            let existing = codex_marker::normalize_existing(authority, existing)?;
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
                if codex_marker::existing_source_is_live(
                    tmux_session_name,
                    &existing,
                    &path,
                    session_id.as_deref(),
                )? {
                    if crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(
                        tmux_session_name,
                    ) != Some(channel_id)
                    {
                        crate::services::tui_prompt_dedupe::register_tmux_channel_under_source_authority(authority, channel_id);
                    }
                    return None;
                }
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
                    // A new relay namespace must not skip unread rollout bytes.
                    fresh.last_offset = existing.last_offset;
                }
                return Some((fresh, true));
            }
            rehydrated_codex_tui_binding_for_tmux_session(
                (authority, epoch),
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
        (
            tmux_session_name,
            &TmuxMirrorEpoch::capture(tmux_session_name)?,
        ),
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
    deferred: bool,
    live_pane: bool,
    canonical_cwd: Option<PathBuf>,
    existing_binding_path: Option<PathBuf>,
    marker_path: Option<PathBuf>,
}

#[cfg(unix)]
fn codex_tui_rehydrate_observations(
    sessions: &[String],
    hosts: &mut MirrorHosts,
) -> Vec<CodexTuiRehydrateObservation> {
    let mut observations = Vec::new();
    for tmux_session_name in sessions {
        let Some(host) = hosts.get_mut(tmux_session_name) else {
            continue;
        };
        let mut live_pane = false;
        let mut canonical_cwd = None;
        if let MirrorHost::Tmux(epoch) = &*host
            && tmux_session_is_codex_tui(tmux_session_name)
        {
            match epoch.probe(tmux_session_name, || {
                codex_pass_pane_is_live(tmux_session_name)
            }) {
                Some(true) => {
                    if let Some(cwd) = epoch.probe(tmux_session_name, || {
                        crate::services::session_host::TmuxHost.pane_current_path(tmux_session_name)
                    }) {
                        live_pane = true;
                        canonical_cwd = cwd
                            .map(PathBuf::from)
                            .map(|cwd| std::fs::canonicalize(&cwd).unwrap_or(cwd));
                    } else {
                        *host = MirrorHost::Deferred;
                    }
                }
                Some(false) => {}
                None => *host = MirrorHost::Deferred,
            }
        }
        let marker_path =
            crate::services::codex_tui::session::read_codex_tui_rollout_marker(tmux_session_name)
                .and_then(|marker| {
                    let path = &marker.rollout_path;
                    (path.is_file()
                        && !crate::services::codex_tui::rollout_index::rollout_is_subagent(path))
                    .then(|| canonical_rollout_claim_path(path))
                });
        observations.push(CodexTuiRehydrateObservation {
            tmux_session_name: tmux_session_name.clone(),
            deferred: matches!(host, MirrorHost::Deferred),
            live_pane,
            canonical_cwd,
            existing_binding_path: codex_tui_existing_valid_binding_path(tmux_session_name),
            marker_path,
        });
    }
    observations
}

#[cfg(unix)]
fn codex_tui_rehydrate_plan(sessions: &[String], hosts: &mut MirrorHosts) -> CodexTuiRehydratePlan {
    codex_tui_rehydrate_plan_from_observations(codex_tui_rehydrate_observations(sessions, hosts))
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
    let unknown_claimant = observations.iter().any(|observation| {
        observation.deferred
            && observation.existing_binding_path.is_none()
            && observation
                .marker_path
                .as_ref()
                .is_none_or(|path| duplicate_marker_paths.contains(path))
    });
    let markerless_fallback_allowed_sessions = if unknown_claimant {
        HashSet::new()
    } else {
        markerless_codex_tui_fallback_allowed_from_cwds(markerless_cwd_by_tmux)
    };

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
    path.is_file().then(|| canonical_rollout_claim_path(&path))
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
    let (relay_output_path, relay_last_offset) =
        codex_marker::relay_checkpoint(tmux_session_name, rollout_path);
    let session_id = session_id.or_else(|| codex_rollout_session_id(rollout_path));
    let input_start = codex_marker::bootstrap(tmux_session_name, rollout_path, session_id.clone())?;
    Some(crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::CodexTui,
        output_path: rollout_path.display().to_string(),
        relay_output_path,
        input_fifo_path: None,
        session_id,
        last_offset: input_start,
        relay_last_offset,
    })
}

#[cfg(unix)]
fn rehydrated_codex_tui_binding_for_tmux_session(
    pane: (
        &crate::services::tmux_common::TmuxSourceAuthority<'_>,
        &TmuxMirrorEpoch,
    ),
    claimed_rollout_paths: &HashSet<PathBuf>,
    reserved_rollout_paths: &HashSet<PathBuf>,
    duplicate_marker_paths: &HashSet<PathBuf>,
    allow_markerless_cwd_fallback: bool,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    let (authority, epoch) = pane;
    let tmux_session_name = authority.session();
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
    if !epoch.is_current(authority) {
        return None;
    }
    let cwd = crate::services::session_host::TmuxHost.pane_current_path(tmux_session_name)?;
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
mod discovery_pass_tests;

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
            deferred: false,
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
        let host_deferred = CodexTuiRehydrateObservation {
            deferred: true,
            live_pane: false,
            ..codex_tui_observation(
                "AgentDesk-codex-marker-backed",
                None,
                None,
                Some(marker_path),
            )
        };
        let plan = codex_tui_rehydrate_plan_from_observations(vec![
            codex_tui_observation("AgentDesk-codex-markerless", Some("/repo"), None, None),
            host_deferred,
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
        use crate::db::dispatched_sessions::hosted_execution::test_support::{
            future_schema, owner, record, wire,
        };
        use crate::services::discord::host_defer_gate::tests::{
            Case, ScriptedTmux, postgres, with_second_bot,
        };
        use crate::services::discord::host_teardown_gate::test_support::Stored;
        use crate::services::tui_prompt_dedupe as dedupe;

        fn binding(kind: RuntimeHandoffKind) -> dedupe::TuiRuntimeBinding {
            dedupe::TuiRuntimeBinding {
                runtime_kind: kind,
                output_path: "/runtime/p4c1-rehydrate.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            }
        }

        // Probe only tmux sessions, and evict only dead orphans or confirmed Herdr mirrors.
        #[test]
        fn a_dead_pane_mirror_is_evicted_only_for_what_the_host_guard_admits_pg() {
            let _root = crate::config::TestRuntimeRootGuard::new();
            let _dedupe = dedupe::TEST_LOCK
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                dedupe::reset_state_for_tests();
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
                            match case {
                                Case::Stored(Stored::Missing) => {}
                                Case::Stored(Stored::MissingHerdrMarker) => {
                                    case.seed(&pool, &key, &name, channel).await;
                                }
                                _ => {
                                    let mut row_owner = owner(&channel.to_string());
                                    row_owner.provider = provider.as_str().to_string();
                                    row_owner.discord_token_hash = shared.token_hash.clone();
                                    let mut hosted = record(&row_owner, "fixture", HostedState::Bound);
                                    hosted.expected.as_mut().unwrap().binding_provider =
                                        row_owner.provider.clone();
                                    let raw = match case {
                                        Case::Stored(Stored::Hosted) => Some(wire(&hosted)),
                                        Case::Stored(Stored::Future) => Some(future_schema(&row_owner)),
                                        Case::Conflict => {
                                            hosted.owner.channel_id = "1".to_string();
                                            hosted.source_ref.channel = "1".to_string();
                                            Some(wire(&hosted))
                                        }
                                        _ => None,
                                    };
                                    let seed = crate::services::discord::host_key_derivation::tests::seed_row;
                                    seed(&pool, provider.as_str(), Some(&shared.token_hash), &key, channel, raw).await;
                                    if case == Case::Stored(Stored::LegacyHerdrMarker) {
                                        Case::Stored(Stored::MissingHerdrMarker)
                                            .seed(&pool, &key, &name, channel).await;
                                    }
                                }
                            }
                            if listed && *kind == RuntimeHandoffKind::CodexTui {
                                crate::services::tmux_common::write_tmux_runtime_kind_marker(&name, *kind)
                                    .unwrap();
                            }
                            dedupe::register_tmux_runtime_binding(&name, binding(*kind));
                            dedupe::register_tmux_channel(&name, channel);
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
                    dedupe::register_tmux_runtime_binding(&name, tui);
                    dedupe::register_tmux_channel(&name, channel);
                    second.push((name, matches!(stored, Stored::Legacy | Stored::Hosted)));
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
                let live = &sessions[0].0;
                tmux.live(live);
                tmux.list(&listed);
                let pass = shared.clone();
                tokio::task::spawn_blocking(move || {
                    rehydrate_existing_claude_tui_bindings(&pass);
                    rehydrate_existing_codex_tui_bindings(&pass);
                })
                .await
                .expect("rehydrate pass");
                assert_eq!(rows(keys()).await, rows_before, "eviction is memory only");
                let calls = tmux.take_calls();
                for (name, _, listed, case) in &sessions {
                    let mirrored = dedupe::runtime_binding_for_tmux_session(name).is_some();
                    let owned = shared
                        .tmux_watchers
                        .owner_channel_for_tmux_session(name)
                        .is_some();
                    let label = format!("{name} listed={listed} {case:?}");
                    let herdr = matches!(case, Case::Stored(
                        Stored::Hosted | Stored::MissingHerdrMarker | Stored::LegacyHerdrMarker
                    ));
                    let probes = calls.iter().any(|call| call.split_whitespace().any(|arg| {
                        arg.strip_prefix('=').unwrap_or(arg).trim_end_matches(':') == name
                    }));
                    assert_eq!(probes, case.admitted(), "{label}: tmux probe");
                    let evicted = herdr || (case.admitted() && name != live);
                    assert_eq!(mirrored, !evicted, "{label}: mirror");
                    assert_eq!(owned, !evicted, "{label}: restored owner");
                }
                for (name, admitted) in &second {
                    let mirrored = dedupe::runtime_binding_for_tmux_session(name).is_some();
                    assert_eq!(mirrored, !admitted, "{name}: second bot's mirror");
                }

                let claude_tui = binding(RuntimeHandoffKind::ClaudeTui);
                dedupe::register_tmux_runtime_binding(unread, claude_tui);
                dedupe::register_tmux_channel(unread, unread_channel);
                pool.close().await;
                let pass = shared.clone();
                tokio::task::spawn_blocking(move || rehydrate_existing_claude_tui_bindings(&pass))
                    .await
                    .expect("rehydrate pass");
                let kept = dedupe::runtime_binding_for_tmux_session(unread);
                assert!(kept.is_some(), "a failed row read is not a legacy answer");
                db.drop().await;
            });
        }

        #[test]
        fn mirror_warning_rearms_after_the_session_leaves_rehydration() {
            let _root = crate::config::TestRuntimeRootGuard::new();
            let _dedupe = dedupe::TEST_LOCK
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            dedupe::reset_state_for_tests();
            let _tmux = ScriptedTmux::install();
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let pool = {
                let _entered = runtime.enter();
                sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://fixture@127.0.0.1:1/fixture")
                    .unwrap()
            };
            let shared =
                crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
            let name = "AgentDesk-claude-mirror-warning-lifetime";
            let log = tempfile::NamedTempFile::new().unwrap();
            let writer = log.reopen().unwrap();
            crate::logging::test_capture::pin_callsite_interest();
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || writer.try_clone().unwrap())
                .finish();
            let tick = || rehydrate_existing_claude_tui_bindings(&shared);
            tracing::subscriber::with_default(subscriber, || {
                for incarnation in 1..=2 {
                    dedupe::register_tmux_runtime_binding(
                        name,
                        binding(RuntimeHandoffKind::ClaudeTui),
                    );
                    tick();
                    tick();
                    let warnings = std::fs::read_to_string(log.path())
                        .unwrap()
                        .lines()
                        .filter(|line| {
                            line.contains(name) && line.contains("no runtime for the host check")
                        })
                        .count();
                    assert_eq!(warnings, incarnation);
                    assert!(dedupe::evict_dead_tmux_mirror(name));
                    tick();
                }
            });
            dedupe::reset_state_for_tests();
        }

        #[test]
        fn mirror_host_timeout_keeps_and_retries_with_one_warning_until_success_pg() {
            let _root = crate::config::TestRuntimeRootGuard::new();
            let _dedupe = dedupe::TEST_LOCK
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                dedupe::reset_state_for_tests();
                let tmux = ScriptedTmux::install();
                let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
                let pool = db.connect_and_migrate_with_max_connections(1).await;
                let shared =
                    crate::services::discord::host_teardown_gate::test_support::shared_on(&pool)
                        .await;
                let name = "AgentDesk-claude-mirror-timeout-fixture";
                let register = || {
                    dedupe::register_tmux_runtime_binding(
                        name,
                        binding(RuntimeHandoffKind::ClaudeTui),
                    )
                };
                let mirrored = || dedupe::runtime_binding_for_tmux_session(name).is_some();
                let log = tempfile::NamedTempFile::new().unwrap();
                let writer = log.reopen().unwrap();
                crate::logging::test_capture::pin_callsite_interest();
                let dispatch = tracing::Dispatch::new(
                    tracing_subscriber::fmt()
                        .with_ansi(false)
                        .with_writer(move || writer.try_clone().unwrap())
                        .finish(),
                );
                let tick = || {
                    let shared = shared.clone();
                    let dispatch = dispatch.clone();
                    tokio::task::spawn_blocking(move || {
                        tracing::dispatcher::with_default(&dispatch, || {
                            evict_dead_orphaned_claude_tui_mirrors(&shared);
                        });
                    })
                };
                let warnings = || {
                    std::fs::read_to_string(log.path())
                        .unwrap()
                        .lines()
                        .filter(|line| {
                            line.contains(name)
                                && line.contains("host check timed out; mirror kept")
                        })
                        .count()
                };
                register();
                let held = pool.acquire().await.unwrap();
                for _ in 0..2 {
                    tick().await.unwrap();
                    assert!(mirrored());
                }
                let calls = tmux.take_calls();
                assert_eq!((warnings(), calls.len()), (1, 0), "{calls:?}");
                drop(held);
                tick().await.unwrap();
                assert!(!mirrored(), "the next tick must retry the host lookup");
                assert!(!tmux.take_calls().is_empty());
                register();
                let held = pool.acquire().await.unwrap();
                tick().await.unwrap();
                assert!(mirrored());
                assert!(tmux.take_calls().is_empty());
                assert_eq!(warnings(), 2, "success must re-arm the warning");
                drop(held);
                pool.close().await;
                db.drop().await;
            });
        }
    }
}
