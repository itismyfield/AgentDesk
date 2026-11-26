//! A queue absorption consumes one registration for notice attribution.
use super::*;
use crate::services::tui_o::store::ledger::input_notices::Settled;

pub(super) type Proven = HashMap<(u64, u64), String>;
type Reserved = HashMap<String, u64>;

pub(crate) type AbsorptionProofs = HashMap<(u64, u64), ([u8; 32], String)>;
pub(crate) type AbsorptionLookup<'a> =
    dyn Fn(&NoticeOrigin, &Fingerprint) -> Option<AbsorptionProofs> + 'a;

pub(super) fn collect(
    store: &ChannelStore,
    origin: &NoticeOrigin,
    entries: &[Entry],
    blocked: &Blocked,
    source_stamp: Option<&Fingerprint>,
    lookup: Option<&AbsorptionLookup<'_>>,
) -> (Proven, Reserved) {
    let state = store.ledger().input_notices();
    let mut reserved = Reserved::new();
    let mut pending = Vec::new();
    for (id, evidence) in &state.evidence {
        if evidence.record.source != origin.record.source || !evidence.is_absorbed_remove() {
            continue;
        }
        if let Some(settlement) = state.settled.get(id) {
            if let Settled::Owned { owner, .. } = &settlement.proof {
                reserve(&mut reserved, owner, id.1);
            }
        } else if evidence
            .origin
            .as_deref()
            .is_some_and(|e| same_scope(e, origin))
            && !state.terminal(store.ledger(), id, evidence)
            && evidence.prompt.as_ref().is_some_and(|prompt| {
                let hashes = class_hashes(prompt);
                entries.iter().any(|e| {
                    !e.id.starts_with("legacy:")
                        && e.append.is_none()
                        && hashes.contains(&e.class_hash)
                })
            })
        {
            pending.push(evidence);
        }
    }
    let mut proven = Proven::new();
    if let Some(source_stamp) = source_stamp
        && !pending.is_empty()
        && let Some(by_range) = lookup.and_then(|lookup| lookup(origin, source_stamp))
    {
        for evidence in pending {
            let range = (evidence.record.start, evidence.record.end);
            let Some((digest, owner)) = by_range.get(&range) else {
                continue;
            };
            let raw_hash: [u8; 32] = Sha256::digest(&evidence.raw_record).into();
            if *digest != raw_hash || reserved.contains_key(owner) {
                continue;
            }
            let Some(entry) = entries
                .iter()
                .find(|e| e.id == *owner && e.append.is_none())
            else {
                continue;
            };
            let mut matching = entries.iter().filter(|e| {
                e.offset <= range.0
                    && e.class_hash == entry.class_hash
                    && reserved.get(&e.id).is_none_or(|at| *at >= range.0)
            });
            if matching.next().is_none_or(|e| e.id != *owner)
                || matching.next().is_some()
                || blocked.iter().any(|(hash, _)| *hash == entry.class_hash)
            {
                continue;
            }
            reserve(&mut reserved, owner, range.0);
            proven.insert(range, owner.clone());
        }
    }
    (proven, reserved)
}

fn reserve(reserved: &mut Reserved, owner: &str, offset: u64) {
    reserved
        .entry(owner.into())
        .and_modify(|before| *before = (*before).min(offset))
        .or_insert(offset);
}
