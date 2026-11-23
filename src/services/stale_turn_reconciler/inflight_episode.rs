//! What the persisted inflight records say about one tmux session's episode.

use crate::services::provider::ProviderKind;

/// Reads which persisted inflight record claims `tmux_name`; the owner runtime supplies it.
pub(crate) type InflightEpisodeProbe = fn(&ProviderKind, &str) -> InflightEpisodeLookup;

/// Stable identity of one persisted inflight episode.
///
/// Progress fields are deliberately excluded. `current_msg_id`, `last_offset`,
/// and the state file's mtime/size all move while the same turn stays
/// authoritative, so an identity built from them would report a transition on
/// every tick of a live turn and starve recovery.
///
/// `started_at` and `user_msg_id` have no production in-episode assignment site
/// and are the required axes. `turn_nonce` and `turn_start_offset` are
/// auxiliary — each fires only when BOTH observations carry a value — because
/// each has production writers that fill it in or restamp it on a row that is
/// already on disk: `turn_nonce` at synthetic episode start, and
/// `turn_start_offset` at `tui_prompt_relay/synthetic_start.rs` and
/// `tui_prompt_relay/codex_idle_rollout.rs`, both of which restamp the offset
/// when a runtime (re)binds to the episode. Firing there is correct rather than
/// spurious — a runtime just claimed the row — and one held tick is not a
/// denial, because the next tick re-baselines on the restamped value.
///
/// `turn_start_offset` is carried because the canonical
/// `inflight::InflightTurnIdentity` already carries it for a collision this
/// repository has proven: two consecutive `user_msg_id == 0` TUI-direct turns
/// collide on `started_at`'s one-second resolution, and a legacy row may have
/// no `turn_nonce` to break the tie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InflightEpisodeIdentity {
    pub(crate) started_at: String,
    pub(crate) user_msg_id: u64,
    pub(crate) turn_nonce: Option<String>,
    pub(crate) turn_start_offset: Option<u64>,
}

/// What the persisted inflight records say about one tmux session.
///
/// The three failure shapes are kept apart from `Unclaimed` on purpose: a read
/// that could not happen, and a claim that is not attributable to one episode,
/// are both "we do not know", and neither is evidence that no turn is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InflightEpisodeLookup {
    /// Exactly one non-`rebind_origin` row claims this tmux session.
    Claimed(InflightEpisodeIdentity),
    /// The store was readable and no row claims this tmux session.
    Unclaimed,
    /// The runtime root could not be resolved, or provider enumeration / a
    /// candidate row could not be read or revalidated under its lock.
    Unprobeable,
    /// More than one row claims this tmux session, so the loader's
    /// `inflight_tmux_one_to_one` invariant is broken. No single row is
    /// authoritative, and which one a directory scan yields first is not
    /// stable, so picking one would make the observation nondeterministic in
    /// both directions: it could mask a live claim behind a stale one, or
    /// invent a transition between two rows that both stayed put.
    Ambiguous,
}
