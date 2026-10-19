//! The Hold leaf behind an Unbound `session_identity_unresolved` answer, logged once per channel
//! until another answer settles; only labels, counts, booleans and tmux session names.
use super::*;
use std::sync::{LazyLock, Mutex};

const IDENTITY_UNRESOLVED: &str = "session_identity_unresolved";
const LISTED_OWNERS: usize = 16;

/// Last leaf logged per channel; absent once the channel settles on any other answer.
static LOGGED: LazyLock<Mutex<BTreeMap<u64, &'static str>>> = LazyLock::new(Default::default);

/// The Hold leaf and the judgment inputs it was decided from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HoldLeaf {
    leaf: &'static str,
    candidates: usize,
    current: bool,
    conflict: bool,
    inflight: bool,
    exact: &'static str,
    canonical: &'static str,
    exact_matches_canonical: bool,
    inventory: &'static str,
    unresolved_owners: Vec<String>,
}

impl HoldLeaf {
    pub(super) fn of(
        leaf: &'static str,
        candidates: &Result<Candidates, String>,
        exact: &HostedLookup,
        canonical: &HostedLookup,
        inventory: &Inventory,
        owners: &BTreeMap<String, Option<u64>>,
    ) -> Self {
        let candidates = candidates.as_ref().ok();
        let namespace = format!("{}-", crate::services::provider::TMUX_SESSION_PREFIX);
        let (inventory, names) = match inventory {
            Inventory::Sessions(names) => ("sessions", names.as_slice()),
            Inventory::Empty(_) => ("empty", &[][..]),
            Inventory::ProbeFailed(_) => ("probe_failed", &[][..]),
        };
        Self {
            leaf,
            candidates: candidates.map_or(0, |c| c.names.len()),
            current: candidates.is_some_and(|c| c.current.is_some()),
            conflict: candidates.is_some_and(|c| c.conflict),
            inflight: candidates.is_some_and(|c| c.inflight),
            exact: lookup_label(exact),
            canonical: lookup_label(canonical),
            exact_matches_canonical: exact == canonical,
            inventory,
            unresolved_owners: (names.iter())
                .filter(|name| name.starts_with(&namespace))
                .filter(|name| !matches!(owners.get(*name), Some(Some(_))))
                .cloned()
                .collect(),
        }
    }
}

fn lookup_label(lookup: &HostedLookup) -> &'static str {
    match lookup {
        HostedLookup::Found(_) => "found",
        HostedLookup::Missing => "missing",
        HostedLookup::Unknown(_) => "unknown",
        HostedLookup::Conflict(_) => "conflict",
    }
}

/// Logs `hold` when `answer` is `session_identity_unresolved` under a leaf not yet logged for
/// `channel`; a superseded answer keeps the last leaf, any other answer re-arms the channel.
pub(super) fn settle(channel: u64, answer: &'static str, hold: Option<&HoldLeaf>) {
    if answer == "superseded" {
        return;
    }
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(hold) = hold.filter(|_| answer == IDENTITY_UNRESOLVED) else {
        logged.remove(&channel);
        return;
    };
    if logged.insert(channel, hold.leaf) == Some(hold.leaf) {
        return;
    }
    drop(logged);
    let listed = &hold.unresolved_owners[..hold.unresolved_owners.len().min(LISTED_OWNERS)];
    tracing::info!(
        channel_id = channel,
        leaf = hold.leaf,
        candidates = hold.candidates,
        current = hold.current,
        conflict = hold.conflict,
        inflight = hold.inflight,
        exact = hold.exact,
        canonical = hold.canonical,
        exact_matches_canonical = hold.exact_matches_canonical,
        inventory = hold.inventory,
        unresolved_owner_count = hold.unresolved_owners.len(),
        unresolved_owners = ?listed,
        "[turn_presence] unbound hold leaf"
    );
}

/// Re-arms `channel` when a read finds it Bound, so the next Unbound Hold logs again.
pub(in crate::services::discord::turn_presence::activity) fn rearm(channel: u64) {
    LOGGED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&channel);
}
