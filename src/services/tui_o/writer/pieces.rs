//! What the writer owes for each spooled record: the A0 identity, seal and split rules, with the
//! split text kept so it can be posted.

use crate::services::discord::formatting::split_for_shadow;
use crate::services::tui_o::shadow::identity::{RecordFact, UnitContent, classify};
use crate::services::tui_o::shadow::seal::{SealOutcome, SealRegistry};
use crate::services::tui_o::shadow::unit_plan::{UnitPlan, plan};
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, UnitKey, UnitKind};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PieceWork {
    pub unit_key: UnitKey,
    pub index: u32,
    pub payload: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Derived {
    Piece(PieceWork),
    Excluded {
        unit_key: UnitKey,
        reason: String,
    },
    /// A record the identity rules cannot place; the channel stops rather than skip it.
    Blocked {
        reason: String,
    },
}

pub struct UnitDeriver {
    channel: u64,
    provider: ShadowProvider,
    seals: SealRegistry,
}

impl UnitDeriver {
    pub fn new(channel: u64, provider: ShadowProvider) -> Self {
        let seals = SealRegistry::default();
        Self {
            channel,
            provider,
            seals,
        }
    }

    /// Units in record order; a repeated key with the same plan (a fork copy) yields nothing.
    pub fn derive(&mut self, record: &CapturedRecord) -> Vec<Derived> {
        if record.line.iter().all(u8::is_ascii_whitespace) {
            return Vec::new();
        }
        let value = match serde_json::from_slice(&record.line) {
            Ok(value) => value,
            Err(error) => {
                return vec![Derived::Blocked {
                    reason: format!("unparseable record at {}: {error}", record.start),
                }];
            }
        };
        let mut out = Vec::new();
        for fact in classify(self.provider, &value) {
            match fact {
                RecordFact::Unit(native_key, kind, content) => {
                    let (channel_id, provider) = (self.channel, self.provider);
                    let unit_key = UnitKey {
                        channel_id,
                        provider,
                        native_key,
                        kind,
                    };
                    out.extend(self.unit(unit_key, content));
                }
                RecordFact::Blocked(reason) => out.push(Derived::Blocked { reason }),
                RecordFact::Announced(native_key, kind) => {
                    let (channel_id, provider) = (self.channel, self.provider);
                    self.seals.announce(UnitKey {
                        channel_id,
                        provider,
                        native_key,
                        kind,
                    });
                }
                _ => {}
            }
        }
        out
    }

    /// Whether a unit with this native key was already derived or announced here.
    pub fn knows(&self, native_key: &str, kind: UnitKind) -> bool {
        let (channel_id, provider, native_key) = (self.channel, self.provider, native_key.into());
        self.seals.knows(&UnitKey {
            channel_id,
            provider,
            native_key,
            kind,
        })
    }

    /// An announced unit whose sealing record is still to come keeps its spool from GC.
    pub fn has_unsealed(&self) -> bool {
        !self.seals.unsealed().is_empty()
    }

    fn unit(&mut self, unit_key: UnitKey, content: UnitContent) -> Vec<Derived> {
        let planned = match plan(&content) {
            Ok(planned) => planned,
            Err(reason) => return vec![Derived::Blocked { reason }],
        };
        match self.seals.seal(&unit_key, &planned) {
            SealOutcome::First => {}
            SealOutcome::Repeat => return Vec::new(),
            SealOutcome::Conflict => {
                let reason = format!("sealed unit {} reappeared changed", unit_key.native_key);
                return vec![Derived::Blocked { reason }];
            }
        }
        match (planned, content) {
            (UnitPlan::Pieces(_), UnitContent::Payload(text)) => {
                let pieces = split_for_shadow(text.trim()).into_iter().enumerate();
                let piece = |(index, (payload, _)): (usize, (String, usize))| {
                    let index = u32::try_from(index).unwrap_or(u32::MAX);
                    Derived::Piece(PieceWork {
                        unit_key: unit_key.clone(),
                        index,
                        payload,
                    })
                };
                pieces.map(piece).collect()
            }
            (UnitPlan::Excluded(reason), _) => vec![Derived::Excluded {
                unit_key,
                reason: reason.into(),
            }],
            (UnitPlan::Pieces(_), UnitContent::Excluded(reason)) => vec![Derived::Excluded {
                unit_key,
                reason: reason.into(),
            }],
        }
    }
}
