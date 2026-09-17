//! #5957: arm a watcher absence from the routing registry, which outlives the
//! watcher, instead of from the mailbox/inflight authorities that an idle
//! channel empties. See the PR body for the 2026-09-16 forensics.

use std::collections::HashSet;
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::{
    WATCHER_ABSENCE, WatcherAbsenceKey, WatcherAbsenceState, runtime_owning_watcher,
    saturating_age_secs,
};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;

/// Four STALL-WATCHDOG ticks. Normal recovery takes one, so this WARN only ever
/// means "recovery is not happening"; it sits below
/// [`super::WATCHER_ABSENCE_DEADMAN_SECS`] so the two form a ladder.
pub(super) const IDLE_RELAY_ABSENCE_WARN_SECS: u64 = 120;

/// A live TUI session whose routing survived its watcher.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RoutableUnwatchedSession {
    pub(super) channel_id: ChannelId,
    pub(super) tmux_session: String,
}

/// The TUI runtime kind a provider's idle relay binds under; `None` providers
/// have no idle relay to lose.
pub(super) fn tui_runtime_kind_for(provider: &ProviderKind) -> Option<RuntimeHandoffKind> {
    match provider {
        ProviderKind::Claude => Some(RuntimeHandoffKind::ClaudeTui),
        ProviderKind::Codex => Some(RuntimeHandoffKind::CodexTui),
        _ => None,
    }
}

/// PURE arming decision: a session qualifies only when it is a live session of
/// this provider, carries a TUI binding of the matching kind, resolves to an
/// authoritative owner channel (never the dedupe mirror — #3018) and no runtime
/// owns a watcher for it. `already_seen` keeps one channel from arming twice.
pub(super) fn routable_unwatched_sessions(
    provider: &ProviderKind,
    live_sessions: &[String],
    tui_binding_kind: impl Fn(&str) -> Option<RuntimeHandoffKind>,
    authoritative_owner: impl Fn(&str) -> Option<ChannelId>,
    watcher_present: impl Fn(ChannelId) -> bool,
    already_seen: &HashSet<ChannelId>,
) -> Vec<RoutableUnwatchedSession> {
    let Some(expected_kind) = tui_runtime_kind_for(provider) else {
        return Vec::new();
    };
    let mut seen = already_seen.clone();
    let mut candidates = Vec::new();
    for tmux_session in live_sessions {
        if !session_belongs_to_provider(provider, tmux_session) {
            continue;
        }
        if tui_binding_kind(tmux_session) != Some(expected_kind) {
            continue;
        }
        let Some(channel_id) = authoritative_owner(tmux_session) else {
            continue;
        };
        if watcher_present(channel_id) {
            continue;
        }
        if !seen.insert(channel_id) {
            continue;
        }
        candidates.push(RoutableUnwatchedSession {
            channel_id,
            tmux_session: tmux_session.clone(),
        });
    }
    candidates
}

/// One host runs Claude and Codex sessions side by side and each provider's
/// watchdog pass sweeps only its own.
fn session_belongs_to_provider(provider: &ProviderKind, tmux_session: &str) -> bool {
    crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session)
        .is_some_and(|(session_provider, _)| &session_provider == provider)
}

/// PURE: one WARN per absence episode, not one per 30 s tick.
pub(super) fn idle_relay_absence_warn_due(absence_secs: u64, announced: bool) -> bool {
    !announced && absence_secs >= IDLE_RELAY_ABSENCE_WARN_SECS
}

/// Arm the absences whose only surviving evidence is the routing registry.
/// Grounds are that entry, never the watcher-stop event: a deliberate
/// `cancel=true` shutdown of a session that is really gone drops out once tmux
/// stops listing it, while one that leaves a live adopted TUI pane behind is
/// exactly the case that must recover. Returns how many absences it observed.
pub(super) async fn observe_routable_unwatched_tui_sessions(
    provider: &ProviderKind,
    runtimes: &[Arc<SharedData>],
    watcher_derived: &std::collections::HashSet<ChannelId>,
    now_unix_secs: i64,
) -> usize {
    if tui_runtime_kind_for(provider).is_none() {
        return 0;
    }
    // One `tmux list-sessions` per pass, off the executor: listing is both the
    // enumeration and the liveness proof. An unavailable tmux yields no
    // candidates rather than a false absence.
    let Ok(Ok(live_sessions)) =
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names).await
    else {
        return 0;
    };
    let candidates = routable_unwatched_sessions(
        provider,
        &live_sessions,
        |tmux_session| {
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session)
                .map(|binding| binding.runtime_kind)
        },
        |tmux_session| {
            runtimes.iter().find_map(|runtime| {
                runtime
                    .tmux_watchers
                    .owner_channel_for_tmux_session(tmux_session)
            })
        },
        |channel_id| runtime_owning_watcher(runtimes, channel_id).is_some(),
        watcher_derived,
    );
    let mut absent = 0usize;
    for candidate in candidates {
        if observe_routable_unwatched_session(provider, &candidate, now_unix_secs) {
            absent += 1;
        }
    }
    absent
}

/// Record one routable-but-unwatched session and announce it past
/// [`IDLE_RELAY_ABSENCE_WARN_SECS`]. The entry lands in the same
/// [`super::WATCHER_ABSENCE`] map the relay-work path uses, so the respawn retry
/// later in this very tick picks it up — recovery within one 30 s tick instead
/// of a process restart. Returns whether the channel is now tracked as absent.
pub(super) fn observe_routable_unwatched_session(
    provider: &ProviderKind,
    candidate: &RoutableUnwatchedSession,
    now_unix_secs: i64,
) -> bool {
    let key = WatcherAbsenceKey::new(provider, candidate.channel_id);
    // A channel the relay-work sweep already observed on this tick carries the
    // richer evidence (and the dead-man ERROR); do not double-observe it.
    if observed_on_this_tick(&key, now_unix_secs) {
        return true;
    }
    let mut entry = WATCHER_ABSENCE
        .entry(key)
        .or_insert_with(|| WatcherAbsenceState::newly_absent(now_unix_secs));
    entry.last_seen_unix_secs = now_unix_secs;
    let absence_secs = saturating_age_secs(entry.first_seen_unix_secs, now_unix_secs);
    if idle_relay_absence_warn_due(absence_secs, entry.idle_relay_announced || entry.escalated) {
        entry.idle_relay_announced = true;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            channel_id = candidate.channel_id.get(),
            provider = provider.as_str(),
            tmux_session = %candidate.tmux_session,
            absence_secs,
            warn_after_secs = IDLE_RELAY_ABSENCE_WARN_SECS,
            "  [{ts}] ⚠ STALL-WATCHDOG: channel {} has a live TUI tmux session with authoritative routing but NO watcher for {absence_secs}s — idle relay output is being lost",
            candidate.channel_id.get(),
        );
    }
    true
}

/// Both sweeps stamp `last_seen_unix_secs` with the pass's single `now`, so
/// equality is the same-tick test.
pub(super) fn observed_on_this_tick(key: &WatcherAbsenceKey, now_unix_secs: i64) -> bool {
    WATCHER_ABSENCE
        .get(key)
        .is_some_and(|state| state.last_seen_unix_secs == now_unix_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str) -> String {
        name.to_string()
    }

    /// The incident fixture: watcher gone, tmux session alive, registry entry
    /// and TUI binding both surviving, mailbox and inflight both empty.
    fn incident_sessions() -> Vec<String> {
        vec![session("AgentDesk-claude-adk-cc")]
    }

    const ADK_CC_CHANNEL: u64 = 1_479_671_298_497_183_835;

    #[test]
    fn live_session_with_surviving_registry_entry_and_no_watcher_is_armed() {
        let armed = routable_unwatched_sessions(
            &ProviderKind::Claude,
            &incident_sessions(),
            |_| Some(RuntimeHandoffKind::ClaudeTui),
            |_| Some(ChannelId::new(ADK_CC_CHANNEL)),
            |_| false,
            &HashSet::new(),
        );
        assert_eq!(
            armed,
            vec![RoutableUnwatchedSession {
                channel_id: ChannelId::new(ADK_CC_CHANNEL),
                tmux_session: "AgentDesk-claude-adk-cc".to_string(),
            }],
            "a live TUI session whose routing outlived its watcher must be armed \
             even though no mailbox turn and no inflight row exist"
        );
    }

    #[test]
    fn a_live_watcher_disqualifies_the_session() {
        assert!(
            routable_unwatched_sessions(
                &ProviderKind::Claude,
                &incident_sessions(),
                |_| Some(RuntimeHandoffKind::ClaudeTui),
                |_| Some(ChannelId::new(ADK_CC_CHANNEL)),
                |_| true,
                &HashSet::new(),
            )
            .is_empty(),
            "a watched channel is healthy and must never be armed"
        );
    }

    #[test]
    fn a_session_without_an_authoritative_registry_entry_is_not_armed() {
        assert!(
            routable_unwatched_sessions(
                &ProviderKind::Claude,
                &incident_sessions(),
                |_| Some(RuntimeHandoffKind::ClaudeTui),
                |_| None,
                |_| false,
                &HashSet::new(),
            )
            .is_empty(),
            "without an authoritative owner there is no channel to respawn for, \
             and the dedupe mirror must not be promoted into that role (#3018)"
        );
    }

    #[test]
    fn a_session_without_a_tui_runtime_binding_is_not_armed() {
        assert!(
            routable_unwatched_sessions(
                &ProviderKind::Claude,
                &incident_sessions(),
                |_| None,
                |_| Some(ChannelId::new(ADK_CC_CHANNEL)),
                |_| false,
                &HashSet::new(),
            )
            .is_empty(),
            "an unadopted pane is not an idle-relay target"
        );
    }

    #[test]
    fn a_foreign_provider_session_is_not_armed() {
        let armed = routable_unwatched_sessions(
            &ProviderKind::Claude,
            &[session("AgentDesk-codex-adk-cc")],
            |_| Some(RuntimeHandoffKind::ClaudeTui),
            |_| Some(ChannelId::new(ADK_CC_CHANNEL)),
            |_| false,
            &HashSet::new(),
        );
        assert!(
            armed.is_empty(),
            "each provider's pass sweeps only its own sessions"
        );
    }

    #[test]
    fn a_non_agentdesk_session_is_not_armed() {
        let armed = routable_unwatched_sessions(
            &ProviderKind::Claude,
            &[session("my-scratch-shell")],
            |_| Some(RuntimeHandoffKind::ClaudeTui),
            |_| Some(ChannelId::new(ADK_CC_CHANNEL)),
            |_| false,
            &HashSet::new(),
        );
        assert!(
            armed.is_empty(),
            "an unrelated pane is never a relay target"
        );
    }

    #[test]
    fn a_binding_of_the_wrong_runtime_kind_is_not_armed() {
        let armed = routable_unwatched_sessions(
            &ProviderKind::Claude,
            &incident_sessions(),
            |_| Some(RuntimeHandoffKind::CodexTui),
            |_| Some(ChannelId::new(ADK_CC_CHANNEL)),
            |_| false,
            &HashSet::new(),
        );
        assert!(
            armed.is_empty(),
            "a Codex binding under a Claude name is drift, not a Claude relay target"
        );
    }

    #[test]
    fn a_channel_the_caller_already_covered_is_not_armed_twice() {
        let mut seen = HashSet::new();
        seen.insert(ChannelId::new(ADK_CC_CHANNEL));
        assert!(
            routable_unwatched_sessions(
                &ProviderKind::Claude,
                &incident_sessions(),
                |_| Some(RuntimeHandoffKind::ClaudeTui),
                |_| Some(ChannelId::new(ADK_CC_CHANNEL)),
                |_| false,
                &seen,
            )
            .is_empty(),
            "the relay-work sweep already covered this channel on this tick"
        );
    }

    #[test]
    fn two_sessions_mapping_to_one_channel_arm_it_once() {
        let armed = routable_unwatched_sessions(
            &ProviderKind::Claude,
            &[
                session("AgentDesk-claude-adk-cc"),
                session("AgentDesk-claude-adk-cc-t42"),
            ],
            |_| Some(RuntimeHandoffKind::ClaudeTui),
            |_| Some(ChannelId::new(ADK_CC_CHANNEL)),
            |_| false,
            &HashSet::new(),
        );
        assert_eq!(
            armed.len(),
            1,
            "the absence map is keyed provider+channel, so one channel arms once"
        );
    }

    #[test]
    fn a_provider_without_a_tui_runtime_arms_nothing() {
        assert!(
            routable_unwatched_sessions(
                &ProviderKind::Gemini,
                &[session("AgentDesk-gemini-adk-cc")],
                |_| Some(RuntimeHandoffKind::ClaudeTui),
                |_| Some(ChannelId::new(ADK_CC_CHANNEL)),
                |_| false,
                &HashSet::new(),
            )
            .is_empty(),
            "no TUI runtime means no idle relay to lose"
        );
    }

    #[test]
    fn the_warn_waits_for_the_threshold_then_fires_once() {
        assert!(
            !idle_relay_absence_warn_due(0, false),
            "a gap the next tick heals must stay quiet"
        );
        assert!(
            !idle_relay_absence_warn_due(IDLE_RELAY_ABSENCE_WARN_SECS - 1, false),
            "below the threshold is still quiet"
        );
        assert!(
            idle_relay_absence_warn_due(IDLE_RELAY_ABSENCE_WARN_SECS, false),
            "an unrecovered gap must announce itself at the threshold"
        );
        assert!(
            !idle_relay_absence_warn_due(IDLE_RELAY_ABSENCE_WARN_SECS * 200, true),
            "one WARN per episode; the 5 h gap must not become 600 lines"
        );
    }

    /// The incident's own duration: 4h44m of absence has to be loud.
    #[test]
    fn the_incident_gap_would_have_announced_itself() {
        let gap_secs = 4 * 3600 + 44 * 60;
        assert!(
            idle_relay_absence_warn_due(gap_secs, false),
            "the 2026-09-16 gap passed silently; it must not be able to again"
        );
    }
}
