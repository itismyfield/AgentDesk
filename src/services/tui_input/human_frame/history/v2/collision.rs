//! Native collision candidates, kept apart from live slot accounting: returning a slot after a
//! local submit never releases a candidate; only a native event or an exact abandon does.
use super::*;

/// Passive state of a registration the queue proved was never sent; it can own no native row.
pub(super) const ABANDONED: &str = "abandoned";

/// Whether another registration could still own the native row `entry` is being matched to.
pub(super) fn contested(
    state: &Snapshot,
    (namespace, key, registration, entry): (&str, &Value, &Registration, &Entry),
    kind: &str,
    offset: u64,
) -> bool {
    let live = state
        .live
        .iter()
        .map(|b| (Some(&b.registration), Some(&b.entry), "live"));
    let planned = state.planned.iter().map(|r| (Some(r), None, "planned"));
    let passive = state
        .passive
        .iter()
        .map(|p| (p.registration.as_ref(), p.entry.as_ref(), p.state.as_str()));
    live.chain(planned)
        .chain(passive)
        .any(|(owner, other, status)| {
            if status == ABANDONED
                || owner.is_some_and(|r| r.namespace == namespace && r.key == *key)
            {
                return false;
            }
            let Some(other) = other else {
                return owner.is_some_and(|r| {
                    r.frame.hash == registration.frame.hash
                        && r.key["attempt"]["target"] == key["attempt"]["target"]
                });
            };
            // Only legacy rows the old writer settled count as natively seen without an event.
            let unconfirmed = status != "settled" || owner.is_some();
            let native = other
                .events
                .iter()
                .any(|event| matches!(event.kind.as_str(), "codex" | "user" | "command"));
            other.path == entry.path
                && other.file == entry.file
                && other.hash == entry.hash
                && other.offset <= offset
                && ((unconfirmed && !native)
                    || (matches!(kind, "command" | "user")
                        && other.events.iter().any(|event| event.kind == "enqueue")))
        })
}
