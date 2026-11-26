//! Read-only authentication of native absorption for notice attribution.
use super::super::binding::BindingEvents;
use super::super::input_facts::native::{Projection, Registration};
use super::super::rotation::Sources;
use super::{NoticeOrigin, ShadowProvider};
use crate::services::tui_input::human_frame::history::{notice_owner::AbsorptionProofs, v2};
use crate::services::tui_o::shadow::{SourceBinding, capture::Fingerprint};
use crate::services::tui_o::store::ChannelStore;
use std::collections::HashMap;

pub(in crate::services::tui_o::writer) fn authenticated_proofs<B: BindingEvents>(
    store: &ChannelStore,
    live: &Sources<B>,
    origin: &NoticeOrigin,
    source_stamp: &Fingerprint,
) -> Option<AbsorptionProofs> {
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
    let mut proofs = if let Some(seed) = live.notice_projection(cursor, source_stamp) {
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
    (Fingerprint::path(source).ok().as_ref() == Some(source_stamp)).then_some(proofs)
}

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
