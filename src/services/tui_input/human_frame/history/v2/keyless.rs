//! A non-scoped writer's change: the format is chosen under the ledger lock, a V1 store keeps its
//! V1 writer, and in a V2 history only keyless entries (no registration) change.
use super::*;
use std::collections::{HashMap, HashSet};

const AMBIGUOUS: &str = "ambiguous ledger identity";
const KEYED: &str = "a non-scoped writer changes no keyed entry";

/// Runs `apply` on the ledger at `path` in one transaction: a V1 file through the V1 writer, a V2
/// or missing one on its owned view.
pub(in super::super) fn update<T>(
    path: &Path,
    apply: impl FnOnce(&mut Store) -> Result<T, String>,
) -> Result<T, String> {
    Registry::shared(path.to_path_buf()).locked(true, |ledger| match ledger {
        Ledger::V1 => super::super::update_v1(path, apply),
        Ledger::V2(state) => reconcile(state, apply),
    })
}

/// Applies `apply`'s change of the owned view to keyless slots, matched by (id, path); a keyed
/// change, a repeated identity or a first native event a keyed registration may own refuses all.
fn reconcile<T>(
    state: &mut Snapshot,
    apply: impl FnOnce(&mut Store) -> Result<T, String>,
) -> Result<T, String> {
    let owned = owned(state);
    let mut before = HashMap::new();
    for (entry, slot) in &owned {
        let bytes = serde_json::to_string(entry).map_err(|e| e.to_string())?;
        let identity = (entry.id.clone(), entry.path.clone());
        if before
            .insert(identity, (bytes, *slot, entry.events.len()))
            .is_some()
        {
            return Err(AMBIGUOUS.into());
        }
    }
    let mut store = Store {
        entries: owned.into_iter().map(|(entry, _)| entry).collect(),
    };
    let result = apply(&mut store)?;
    let (mut changed, mut added, mut seen) = (Vec::new(), Vec::new(), HashSet::new());
    for entry in store.entries {
        let identity = (entry.id.clone(), entry.path.clone());
        if !seen.insert(identity.clone()) {
            return Err(AMBIGUOUS.into());
        }
        let Some((bytes, slot, events)) = before.remove(&identity) else {
            added.push(entry);
            continue;
        };
        if serde_json::to_string(&entry).map_err(|e| e.to_string())? != bytes {
            changed.push((slot.ok_or(KEYED)?, events, entry));
        }
    }
    let mut removed = (before.into_values())
        .map(|(_, slot, _)| slot.ok_or(KEYED))
        .collect::<Result<Vec<_>, _>>()?;
    let new_events = (changed.iter().map(|(_, events, entry)| (*events, entry)))
        .chain(added.iter().map(|entry| (0, entry)));
    for (events, entry) in new_events {
        let start = (entry.append.as_ref())
            .and_then(append::State::receipt_start)
            .unwrap_or(u64::MAX);
        if (entry.events.iter().skip(events))
            .any(|event| collision::contests_migrated(state, entry, &event.kind, start))
        {
            return Err("a same-frame V2 registration may own this native row".into());
        }
    }
    for (at, _, entry) in changed {
        state.passive[at] = keyless(entry);
    }
    removed.sort_unstable_by(|a, b| b.cmp(a));
    for at in removed {
        state.passive.remove(at);
    }
    state.passive.extend(added.into_iter().map(keyless));
    Ok(result)
}

/// The passive shape migration gives the same V1 entry.
fn keyless(entry: Entry) -> Passive {
    Passive {
        registration: None,
        state: if entry.events.is_empty() {
            "retained"
        } else {
            "settled"
        }
        .into(),
        entry: Some(entry),
        encoded: None,
    }
}

#[cfg(all(test, unix))]
#[path = "keyless_tests.rs"]
mod tests;
