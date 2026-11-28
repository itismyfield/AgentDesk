//! Read-only absorption proof for notice settlement.
use super::*;
use crate::services::tui_input::human_frame::history::notice_owner::{
    AbsorptionProofs, ConsumedAbsorptions,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub(super) struct Removal {
    digest: [u8; 32],
    hashes: Vec<String>,
}

pub(super) fn removal(value: &Value, line: &[u8]) -> Option<Box<Removal>> {
    if value["type"] != "queue-operation"
        || value["operation"] != "remove"
        || value["reason"] != "absorbed_mid_turn"
        || value["isMeta"] == true
        || value["isSidechain"] == true
    {
        return None;
    }
    let body = receipt::exact_body(&value["content"])?;
    Some(Box::new(Removal {
        digest: Sha256::digest(line).into(),
        hashes: receipt::exact_hashes(&body),
    }))
}

pub(super) struct NoticeFold<'a> {
    consumed: &'a ConsumedAbsorptions,
    pub(super) unowned: HashMap<(u64, u64), u64>,
}

pub(super) struct Window {
    start: u64,
    end: Option<u64>,
}

fn identity(registration: &Registration) -> String {
    serde_json::json!([registration.namespace, registration.ticket.key]).to_string()
}

impl Window {
    pub(super) fn new(registration: &Registration, notices: Option<&NoticeFold<'_>>) -> Self {
        Self {
            start: registration
                .ticket
                .source
                .as_ref()
                .map_or(u64::MAX, |s| s.offset),
            end: notices.and_then(|n| {
                (!n.consumed.is_empty())
                    .then(|| n.consumed.get(&identity(registration)).copied())
                    .flatten()
            }),
        }
    }

    pub(super) fn contains(&self, offset: u64) -> bool {
        self.start <= offset && self.within_end(offset)
    }

    fn within_end(&self, offset: u64) -> bool {
        self.end.is_none_or(|end| offset < end)
    }

    pub(super) fn accepts_row(&self, registration: &Registration, value: &Fact) -> bool {
        self.within_end(value.range.0)
            && (registration.receipts.is_empty()
                || value
                    .row
                    .as_deref()
                    .is_some_and(|uuid| registration.receipts.iter().any(|known| known == uuid)))
    }

    pub(super) fn unique_enqueue(&self, records: &[Fact], body: &[String]) -> bool {
        records
            .iter()
            .filter(|r| self.contains(r.range.0))
            .filter(|r| matches!(&r.event, Some(Event::Enqueue(b)) if same_body(b, body)))
            .take(2)
            .count()
            == 1
    }
}

impl Projection {
    pub(crate) fn notice_absorptions(
        &self,
        registrations: &[Registration],
        consumed: &ConsumedAbsorptions,
    ) -> Result<AbsorptionProofs, String> {
        let mut notices = NoticeFold {
            consumed,
            unowned: HashMap::new(),
        };
        let analysis = reduce(
            &self.binding,
            (&self.facts, None),
            registrations,
            self.through,
            None,
            Some(&mut notices),
        )?;
        let mut proven = HashMap::new();
        for (&range, &enqueue) in &notices.unowned {
            let Some(remove) = self.removal_at(range) else {
                continue;
            };
            let matching_enqueue = self.facts.iter().any(|fact| {
                fact.range.0 == enqueue
                    && matches!(fact.event, Some(Event::Enqueue(_)))
                    && fact
                        .receipt
                        .as_ref()
                        .is_some_and(|r| same_body(&remove.hashes, &r.hashes))
            });
            if matching_enqueue {
                proven.insert(range, (remove.digest, None));
            }
        }
        for evidence in analysis.evidence {
            let Some(range) = evidence.membership_range else {
                continue;
            };
            if evidence.membership_row.is_some()
                || !matches!(
                    evidence.disposition,
                    Disposition::Executed | Disposition::Cancelled
                )
            {
                continue;
            }
            let Some(registration) = registrations
                .iter()
                .find(|r| r.namespace == evidence.namespace && r.ticket == evidence.ticket)
            else {
                continue;
            };
            let window = Window::new(registration, Some(&notices));
            let Some(remove) = self.removal_at(range) else {
                continue;
            };
            let frame = &registration.exact_frame_hash;
            let exact_enqueue = self.facts.iter().any(|fact| {
                window.contains(fact.range.0)
                    && fact.range.1 <= range.0
                    && matches!(fact.event, Some(Event::Enqueue(_)))
                    && fact
                        .receipt
                        .as_ref()
                        .is_some_and(|r| r.hashes.contains(frame))
            });
            if exact_enqueue && remove.hashes.contains(frame) {
                proven.insert(range, (remove.digest, Some(identity(registration))));
            }
        }
        Ok(proven)
    }

    fn removal_at(&self, range: (u64, u64)) -> Option<&Removal> {
        self.facts
            .iter()
            .find(|fact| fact.range == range)?
            .notice_absorption
            .as_deref()
    }
}
