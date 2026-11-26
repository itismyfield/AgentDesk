//! Read-only absorption proof for notice settlement.
use super::*;
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

type Proofs = HashMap<(u64, u64), ([u8; 32], String)>;

impl Projection {
    pub(crate) fn notice_absorptions(
        &self,
        registrations: &[Registration],
    ) -> Result<Proofs, String> {
        let mut proven = HashMap::new();
        for evidence in self.analyze(registrations)?.evidence {
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
            let Some(source) = evidence.ticket.source.as_ref() else {
                continue;
            };
            let Some(remove) = self
                .facts
                .iter()
                .find(|fact| fact.range == range)
                .and_then(|fact| fact.notice_absorption.as_ref())
            else {
                continue;
            };
            let frame = &registration.exact_frame_hash;
            let exact_enqueue = self.facts.iter().any(|fact| {
                fact.range.0 >= source.offset
                    && fact.range.1 <= range.0
                    && matches!(fact.event, Some(Event::Enqueue(_)))
                    && fact
                        .receipt
                        .as_ref()
                        .is_some_and(|r| r.hashes.contains(frame))
            });
            if exact_enqueue && remove.hashes.contains(frame) {
                let owner =
                    serde_json::json!([evidence.namespace, evidence.ticket.key]).to_string();
                proven.insert(range, (remove.digest, owner));
            }
        }
        Ok(proven)
    }
}
