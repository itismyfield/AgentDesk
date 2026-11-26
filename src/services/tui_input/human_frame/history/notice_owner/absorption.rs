//! A queue absorption consumes one registration for notice attribution.
use super::*;
use crate::services::tui_o::store::ledger::input_notices::Settled;

pub(super) type Proven = HashMap<(u64, u64), String>;
type Reserved = HashMap<String, u64>;

#[cfg(unix)]
use crate::services::tui_o::writer::input_facts::native::{Projection, Registration};

#[cfg(unix)]
pub(crate) trait NativeNoticeSource {
    fn notice_projection(
        &self,
        cursor: &crate::services::tui_o::store::spool::Cursor,
        stamp: &Fingerprint,
    ) -> Option<&Projection>;
}

pub(super) fn collect(
    store: &ChannelStore,
    origin: &NoticeOrigin,
    entries: &[Entry],
    blocked: &Blocked,
    source_stamp: Option<&Fingerprint>,
    #[cfg(unix)] live: Option<&dyn NativeNoticeSource>,
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
        && let Some(by_range) = authenticated_proofs(
            store,
            origin,
            source_stamp,
            #[cfg(unix)]
            live,
        )
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

type SourceProofs = HashMap<(u64, u64), ([u8; 32], String)>;

#[cfg(unix)]
fn authenticated_proofs(
    store: &ChannelStore,
    origin: &NoticeOrigin,
    source_stamp: &Fingerprint,
    live: Option<&dyn NativeNoticeSource>,
) -> Option<SourceProofs> {
    use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding};

    let source = &origin.record.source;
    let cursor = store.cursor(source)?;
    let binding = SourceBinding {
        channel_id: store.init().channel,
        provider: ShadowProvider::Claude,
        source: source.clone(),
    };
    let registry = v2::Registry::runtime().ok()?;
    let mut registrations: Vec<_> = registry
        .native_classification_registrations()
        .ok()?
        .into_iter()
        .filter(|r| {
            r.ticket.channel_id == binding.channel_id
                && (r.matches(&binding) || r.ticket.source.is_none())
        })
        .collect();
    let allowed = verified_attempts(&registry, &mut registrations);
    if allowed.is_empty() {
        return None;
    }
    let mut proofs =
        if let Some(seed) = live.and_then(|live| live.notice_projection(cursor, source_stamp)) {
            seed.notice_absorptions(&registrations).ok()?
        } else {
            let (capture, projection) =
                Projection::reopen(binding, cursor.captured_through, &cursor.prefix_hash).ok()?;
            let proofs = projection.ok()?.notice_absorptions(&registrations).ok()?;
            if capture.fingerprint().ok().as_ref() != Some(source_stamp) {
                return None;
            }
            proofs
        };
    proofs.retain(|range, (_, owner)| {
        allowed
            .get(owner)
            .is_some_and(|expected| expected.is_none_or(|r| r == *range))
    });
    (census::fingerprint(source).as_ref() == Some(source_stamp)).then_some(proofs)
}

#[cfg(unix)]
fn verified_attempts(
    registry: &v2::Registry,
    registrations: &mut [Registration],
) -> HashMap<String, Option<(u64, u64)>> {
    use v2::native::Disposition;
    let mut allowed = HashMap::new();
    for reg in registrations {
        if reg.ticket.source.is_none() || !reg.receipts.is_empty() {
            continue;
        }
        let Ok(settlement) = registry.native_settlement(&reg.namespace, &reg.ticket) else {
            continue;
        };
        if settlement.extra.get("any_key") != Some(&serde_json::Value::Bool(true))
            || settlement.extra.get("not_sent") == Some(&serde_json::Value::Bool(true))
        {
            continue;
        }
        let owner = serde_json::json!([reg.namespace, reg.ticket.key]).to_string();
        if let Some(outcome) = settlement.outcome {
            // A recorded terminal result can only attest to its own membership range.
            let Some(proof) = outcome.extra.get("native_proof") else {
                continue;
            };
            if matches!(
                outcome.disposition,
                Disposition::Executed | Disposition::Cancelled
            ) && proof.get("membership_row") == Some(&serde_json::Value::Null)
                && let Ok(range) =
                    serde_json::from_value::<(u64, u64)>(proof["membership_range"].clone())
                && range.0 < range.1
            {
                reg.candidate = true;
                allowed.insert(owner, Some(range));
            }
        } else if reg.candidate {
            allowed.insert(owner, None);
        }
    }
    allowed
}

#[cfg(not(unix))]
fn authenticated_proofs(
    _: &ChannelStore,
    _: &NoticeOrigin,
    _: &Fingerprint,
) -> Option<SourceProofs> {
    None
}
