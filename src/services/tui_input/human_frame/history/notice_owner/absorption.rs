//! A queue absorption consumes one registration for notice attribution.
use super::*;
use crate::services::tui_o::store::ledger::input_notices::Settled;

pub(super) type Proven = HashMap<(u64, u64), Option<String>>;
pub(crate) type ConsumedAbsorptions = HashMap<String, u64>;

pub(crate) type AbsorptionProofs = HashMap<(u64, u64), ([u8; 32], Option<String>)>;
pub(crate) type AbsorptionLookup<'a> =
    dyn Fn(&NoticeOrigin, &Fingerprint, &ConsumedAbsorptions) -> Option<AbsorptionProofs> + 'a;

pub(super) fn collect(
    store: &ChannelStore,
    origin: &NoticeOrigin,
    entries: &[Entry],
    blocked: &Blocked,
    source_stamp: Option<&Fingerprint>,
    lookup: Option<&AbsorptionLookup<'_>>,
) -> (Proven, ConsumedAbsorptions) {
    let state = store.ledger().input_notices();
    let mut consumed = ConsumedAbsorptions::new();
    let mut pending = Vec::new();
    for (id, evidence) in &state.evidence {
        if evidence.record.source != origin.record.source || !evidence.is_absorbed_remove() {
            continue;
        }
        if let Some(settlement) = state.settled.get(id) {
            if let Settled::Owned { owner, .. } = &settlement.proof {
                reserve(&mut consumed, owner, id.2);
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
    let Some(source_stamp) = source_stamp.filter(|_| !pending.is_empty()) else {
        return (proven, consumed);
    };
    let Some(by_range) = lookup.and_then(|lookup| lookup(origin, source_stamp, &consumed)) else {
        return (proven, consumed);
    };
    let mut reserved = consumed.clone();
    pending.sort_by_key(|evidence| evidence.record.start);
    for evidence in pending {
        let range = (evidence.record.start, evidence.record.end);
        let Some((digest, owner)) = by_range.get(&range) else {
            continue;
        };
        let raw_hash: [u8; 32] = Sha256::digest(&evidence.raw_record).into();
        let hashes = class_hashes(evidence.prompt.as_deref().unwrap_or_default());
        if *digest != raw_hash || blocked.iter().any(|(hash, _)| hashes.contains(hash)) {
            continue;
        }
        let prior = entries
            .iter()
            .filter(|entry| entry.offset <= range.0 && hashes.contains(&entry.class_hash));
        let spent = |id| consumed.get(id).is_some_and(|end| *end <= range.0);
        let mut matching = prior.clone().filter(|entry| !spent(&entry.id));
        match (matching.next(), matching.next(), owner) {
            (Some(entry), None, Some(owner))
                if entry.id == *owner
                    && entry.append.is_none()
                    && !reserved.contains_key(owner) =>
            {
                reserve(&mut reserved, owner, range.1);
                proven.insert(range, Some(owner.clone()));
            }
            (None, None, None) if prior.clone().any(|entry| spent(&entry.id)) => {
                proven.insert(range, None);
            }
            _ => {}
        }
    }
    (proven, reserved)
}

fn reserve(reserved: &mut ConsumedAbsorptions, owner: &str, offset: u64) {
    reserved
        .entry(owner.into())
        .and_modify(|before| *before = (*before).min(offset))
        .or_insert(offset);
}
