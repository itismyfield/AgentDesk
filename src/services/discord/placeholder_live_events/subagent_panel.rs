//! #4367: LIVE status-panel `Subagents` section rendering, split out of
//! `status_panel.rs` to keep that file within the placeholder_live_events
//! namespace size cap (mirrors what `task_panel.rs` does for the Tasks section
//! after #4093). Owns the subagent-slot render helper, the in-progress-only
//! live filter, and (#4396) the `SubagentEnd` fallback slot-matching queries
//! plus their eviction-tombstone guard (`SubagentKeyTombstones`); the
//! `SubagentSlot` model and its lifecycle state machine stay in
//! `status_panel.rs`, and the completion footer keeps its own terminal-aware
//! subagent rendering.

use super::common::{
    EVENT_LINE_MAX_CHARS, STATUS_PANEL_SUBAGENT_LIMIT, escape_status_panel_markdown,
    normalize_summary, sanitized_tool_name, truncate_chars, truncate_chars_with_marker,
};
use super::status_panel::SubagentSlot;
use super::subagent_summary::render_subagent_done_summary;

pub(super) fn render_subagent_slot(slot: &SubagentSlot) -> String {
    let mut line = format!(
        "└ {} {}",
        sanitize_label(&slot.subagent_type),
        escape_status_panel_markdown(&normalize_summary(&slot.desc))
    );
    if let Some(recent) = slot
        .recent
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        line.push_str(" — ");
        line.push_str(&escape_status_panel_markdown(&normalize_summary(recent)));
    }
    // #3086: append the TUI-parity Done summary on finished slots with accounting.
    if let Some(summary) = slot
        .summary
        .as_ref()
        .filter(|_| matches!(slot.finished, Some(true)))
        .filter(|summary| !summary.is_empty())
        && let Some(done) = render_subagent_done_summary(summary)
    {
        line.push_str(" — ");
        line.push_str(&done);
    }
    // #3391: reserve marker width so a finished line always ENDS WITH its ✓/✗.
    match slot.terminal_marker() {
        Some(marker) => truncate_chars_with_marker(&line, marker, EVENT_LINE_MAX_CHARS),
        None => truncate_chars(&line, EVENT_LINE_MAX_CHARS),
    }
}

/// #4367: a subagent slot is "in progress" — the only kind the LIVE Subagents
/// panel now renders — iff it is NOT terminal (carries no ✓/✗). A finished slot
/// (completed / failed) is hidden immediately so finished subagents no longer
/// mask active ones until they fall out of the 10-slot window (the exact #4367
/// symptom: two already-completed relay-audit subagents kept showing).
///
/// `finished == None` is treated as IN PROGRESS, not "done" — the direct analogue
/// of #4093's `status == None` reasoning for tasks. A freshly-created subagent
/// (`SubagentStart`) carries `finished == None` until its `SubagentEnd`, and a
/// `run_in_background` subagent keeps `finished == None` across an ack-only end
/// for its whole running life; only a genuine completion sets `finished =
/// Some(_)`. Keying on terminal-ness (`SubagentSlot::is_terminal`) alone
/// therefore keeps brand-new and long-running subagents visible mid-flight.
///
/// This gates the LIVE panel only. The completion footer deliberately still
/// renders terminal slots — its ✓/✗ turn-end result summary and the #3391
/// delivered-terminal eviction both depend on finished rows being emitted — so it
/// must not use this predicate.
pub(super) fn subagent_slot_is_in_progress(slot: &SubagentSlot) -> bool {
    !slot.is_terminal()
}

/// #4367: renders the LIVE status panel's `Subagents` section for `subagents`, or
/// `None` when nothing should render. Only in-progress slots are shown (completed
/// / failed rows are hidden so they can never mask active work), newest first,
/// capped at `STATUS_PANEL_SUBAGENT_LIMIT` over the FILTERED set. Returns `None`
/// when no in-progress subagent survives so the caller emits no dangling
/// `Subagents` header. The Codex-provider suppression stays with the caller (Codex
/// never renders subagents). Colocated here (mirroring
/// `task_panel::render_live_tasks_section`) so subagent-slot rendering lives with
/// the subagent-slot render helper.
///
/// No #3404 live terminal-slot compaction runs here (nor in the Tasks section
/// after #4093 후속). `compact_live_panel_terminal_lines` classified a line as
/// terminal by TEXT (`ends_with('✓'|'✗')`); once this section is filtered to
/// in-progress slots, no genuine terminal line can reach it, so its only possible
/// matches would be FALSE POSITIVES — a running subagent whose desc/recent text
/// happens to end with a ✓/✗ glyph — which would wrongly hide in-progress rows
/// behind a `… (+N completed)` summary (the #4367 bug inverted). Terminals are
/// hidden outright now, so capping how many terminal rows render is moot.
pub(super) fn render_live_subagents_section(subagents: &[SubagentSlot]) -> Option<String> {
    if subagents.is_empty() {
        return None;
    }
    let lines = subagents
        .iter()
        .rev()
        .filter(|slot| subagent_slot_is_in_progress(slot))
        .take(STATUS_PANEL_SUBAGENT_LIMIT)
        .map(render_subagent_slot)
        .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| format!("Subagents\n{}", lines.join("\n")))
}

fn sanitize_label(raw: &str) -> String {
    sanitized_tool_name(raw).unwrap_or_else(|| "Task".to_string())
}

/// #4177: conservative auxiliary pairing for a genuine `SubagentEnd` that missed
/// its exact `tool_use_id` match (or, #4396, carries no id at all): the UNIQUE
/// key-matched slot — by agent_id, else by desc — and only when that unique
/// owner is still unfinished. Zero or ambiguous candidates match nothing — the
/// caller drops the event rather than guess.
///
/// #4396 r2 (opus review): the uniqueness scan spans FINISHED slots too. A
/// finished slot sharing the key — e.g. a TTL-swept instance A beside a
/// same-desc respawned live B — means the key cannot prove which instance this
/// end belongs to, so it is an ownership conflict: drop, never finalize the
/// live slot, and never fall through from a conflicted agent_id to the weaker
/// desc key.
///
/// #4396 r4 (codex review): the agent_id branch must consult the tombstone ring
/// on BOTH carried keys, symmetric with the desc branch — not only on its own
/// agent_id key. A late id-bearing async `<task-notification>` end carries a
/// `<task-id>` (=agent_id) AND a desc; r3 only tombstone-checked whichever
/// single key each branch matched on, so an end whose agent_id was NEVER
/// tombstoned — its departed owner A was id-less and left the state with only a
/// DESC tombstone — still uniquely agent_id-matched a same-desc live respawn B
/// and wrong-killed it. A fresh tombstone hit on EITHER carried key is the same
/// ownership conflict → conservative drop. Any true residual is ✗-finalized by
/// the 30-min silence TTL sweep (this PR's "residue over wrong-kill" invariant).
///
/// Intended, documented tradeoff: a respawn B that reuses A's agent_id inherits
/// A's own agent_id tombstone (eviction pushes agent_id + desc keys per slot, so
/// B's agent_id key == the tombstoned A agent_id key). B's OWN genuine id-less
/// fallback end is therefore also dropped for the tombstone TTL. Residue over
/// wrong-kill, deliberately — the exact `tool_use_id` match still closes B when
/// its end carries the id (that path runs before this fallback, so id-bearing
/// EXACT completions are unaffected by this guard).
pub(super) fn match_subagent_end_fallback(
    slots: &[SubagentSlot],
    tombstones: &SubagentKeyTombstones,
    agent_id: Option<&str>,
    desc: Option<&str>,
) -> Option<usize> {
    let now = std::time::Instant::now();
    if let Some(agent_id) = clean_match_key(agent_id) {
        // #4396 r4: cross-key tombstone guard — the carried desc may be
        // tombstoned even when the agent_id is not (id-less departed owner).
        if let Some(desc) = clean_match_key(desc)
            && tombstones.contains_fresh(desc, now)
        {
            log_live_owner_conflict(
                "desc",
                desc,
                "a recently evicted slot shares the carried desc (tombstone)",
            );
            return None;
        }
        match unique_live_owner(slots, tombstones, "agent_id", agent_id, now, |slot| {
            slot.agent_id.as_deref() == Some(agent_id)
        }) {
            Ok(Some(index)) => return Some(index),
            Err(()) => return None,
            Ok(None) => {}
        }
    }
    let desc = clean_match_key(desc)?;
    unique_live_owner(slots, tombstones, "desc", desc, now, |slot| {
        slot.desc == desc
    })
    .ok()?
}

/// `Ok(Some)` iff exactly one slot — finished or not — matches the key and that
/// sole owner is unfinished. Any finished match (a sole one is a late duplicate
/// for an already-closed slot; beside a live one it is the #4396 r2
/// finished/live ownership conflict), any fresh tombstone hit (#4396 r3: the
/// finished conflictor already LEFT the state), and any second live match bail
/// with `Err`, logging the key that failed to identify a unique live owner.
fn unique_live_owner(
    slots: &[SubagentSlot],
    tombstones: &SubagentKeyTombstones,
    key_kind: &'static str,
    key: &str,
    now: std::time::Instant,
    mut matches: impl FnMut(&SubagentSlot) -> bool,
) -> Result<Option<usize>, ()> {
    if tombstones.contains_fresh(key, now) {
        log_live_owner_conflict(
            key_kind,
            key,
            "a recently evicted slot shares the key (tombstone)",
        );
        return Err(());
    }
    let mut found = None;
    for (index, slot) in slots.iter().enumerate().rev() {
        if !matches(slot) {
            continue;
        }
        if slot.finished.is_some() || found.is_some() {
            log_live_owner_conflict(
                key_kind,
                key,
                if slot.finished.is_some() {
                    "a finished slot shares the key"
                } else {
                    "multiple live matches"
                },
            );
            return Err(());
        }
        found = Some(index);
    }
    Ok(found)
}

fn log_live_owner_conflict(key_kind: &'static str, key: &str, conflict: &'static str) {
    tracing::info!(
        target: "agentdesk::discord::live_panel",
        key_kind,
        key,
        conflict,
        "#4396: subagent end fallback dropped — key does not identify a unique live owner"
    );
}

/// #4396 r3 (codex review): ring of match keys (agent_id / desc) from subagent
/// slots that recently LEFT this channel's panel state — footer delivered-
/// terminal eviction (#3391), `trim_subagents`, and the turn/session resets.
/// The r2 finished-slot conflict guard only holds while the finished slot is
/// still IN the state; once evicted, a late id-less end for the departed
/// instance A would again uniquely match a same-key live respawn B and
/// wrong-kill it. A fresh tombstone hit is the same ownership conflict → drop.
///
/// Honest residual windows (accepted, documented): (1) capacity — more than
/// `CAPACITY` subagent departures inside one late-arrival window push the
/// oldest tombstones out early, reopening the wrong-kill window for exactly
/// those keys; (2) freshness — a tombstone expires after `TTL`, so an end
/// arriving later still can close a same-key respawn (a notification that
/// stale exceeds any observed delivery delay); (3) whole-state drops
/// (`clear_channel*` removing the channel entry) discard the ring together
/// with the state it guards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SubagentKeyTombstones {
    /// Push order == time order (monotonic `Instant`s), so the front is always
    /// the oldest — both the cap and the TTL prune pop from the front.
    entries: std::collections::VecDeque<(String, std::time::Instant)>,
}

impl SubagentKeyTombstones {
    const CAPACITY: usize = 32;
    const TTL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

    /// Records BOTH match keys of a slot leaving the state (blank keys skipped).
    pub(super) fn push_slot_keys(&mut self, slot: &SubagentSlot, now: std::time::Instant) {
        for key in [slot.agent_id.as_deref(), Some(slot.desc.as_str())] {
            if let Some(key) = clean_match_key(key) {
                self.push_key(key, now);
            }
        }
    }

    fn push_key(&mut self, key: &str, now: std::time::Instant) {
        self.prune_expired(now);
        while self.entries.len() >= Self::CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((key.to_string(), now));
    }

    pub(super) fn contains_fresh(&self, key: &str, now: std::time::Instant) -> bool {
        self.entries
            .iter()
            .any(|(entry, at)| entry == key && now.saturating_duration_since(*at) < Self::TTL)
    }

    fn prune_expired(&mut self, now: std::time::Instant) {
        while let Some((_, at)) = self.entries.front() {
            if now.saturating_duration_since(*at) >= Self::TTL {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }
}

pub(super) fn clean_match_key(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::SubagentKeyTombstones;
    use std::time::{Duration, Instant};

    // #4396 r3 (4): the ring is hard-capped — the (CAPACITY+1)th push evicts the
    // oldest key first; the buffer can never grow past CAPACITY.
    #[test]
    fn tombstone_ring_caps_at_capacity_evicting_oldest_first() {
        let mut ring = SubagentKeyTombstones::default();
        let now = Instant::now();
        for i in 0..=SubagentKeyTombstones::CAPACITY {
            ring.push_key(&format!("key-{i}"), now);
        }
        assert_eq!(
            ring.entries.len(),
            SubagentKeyTombstones::CAPACITY,
            "ring must never exceed its capacity"
        );
        assert!(
            !ring.contains_fresh("key-0", now),
            "the oldest key must be pushed out at the cap"
        );
        assert!(ring.contains_fresh("key-1", now));
        assert!(ring.contains_fresh(&format!("key-{}", SubagentKeyTombstones::CAPACITY), now));
    }

    // #4396 r3: only FRESH tombstones conflict — an entry older than the TTL
    // neither blocks a close nor lingers in the ring past the next push.
    #[test]
    fn tombstone_expires_after_ttl_and_is_pruned_on_push() {
        let mut ring = SubagentKeyTombstones::default();
        let now = Instant::now();
        let stale = now
            .checked_sub(SubagentKeyTombstones::TTL + Duration::from_secs(1))
            .expect("monotonic clock far enough past origin");
        ring.push_key("old", stale);
        assert!(
            !ring.contains_fresh("old", now),
            "an expired tombstone must not conflict"
        );
        ring.push_key("fresh", now);
        assert_eq!(
            ring.entries.len(),
            1,
            "expired entries must be pruned on push"
        );
        assert!(ring.contains_fresh("fresh", now));
    }
}

/// #4396 point 2: match-basis observability for an id-less terminal end — which
/// slot a unique agent_id/desc match closed, or that the event was dropped.
pub(super) fn log_idless_terminal_fallback(
    slots: &[SubagentSlot],
    matched: Option<usize>,
    agent_id: Option<&str>,
    desc: Option<&str>,
) {
    match matched {
        Some(index) => tracing::info!(
            target: "agentdesk::discord::live_panel",
            agent_id = agent_id.unwrap_or(""),
            desc = desc.unwrap_or(""),
            slot_tool_use_id = slots[index].tool_use_id.as_deref().unwrap_or(""),
            slot_ordinal = slots[index].ordinal,
            "#4396: id-less terminal subagent end closed its unique agent_id/desc-matched slot"
        ),
        None => tracing::info!(
            target: "agentdesk::discord::live_panel",
            agent_id = agent_id.unwrap_or(""),
            desc = desc.unwrap_or(""),
            "#4396: id-less terminal subagent end dropped (zero or ambiguous slot match)"
        ),
    }
}
