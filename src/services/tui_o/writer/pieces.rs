//! What the writer owes for each spooled record: the A0 identity, seal and split rules, with the
//! split text kept so it can be posted.

pub use super::tool_cards::ToolCardInputs;
use super::tool_cards::{ToolCardRun, ToolMeta};
pub use super::turn_summary::SummaryInputs;
use super::turn_summary::TurnSummaryRun;
use crate::services::discord::formatting::split_for_shadow;
use crate::services::tui_o::shadow::identity::{RecordFact, UnitContent, classify_with_version};
use crate::services::tui_o::shadow::seal::{SealOutcome, SealRegistry};
use crate::services::tui_o::shadow::unit_plan::{UnitPlan, plan};
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, UnitKey, UnitKind};

/// Ledger reason for a tool call left to Legacy's live panel.
pub const TOOL_CALL_PANEL: &str = "tool_call_panel";
pub const TOOL_CALL_CARD: &str = "tool_call_card";

/// Raw tool output is internal, including a command's nonfatal failure.
pub const TOOL_RESULT_INTERNAL: &str = "tool_result_internal";

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwedWork {
    pub derived: Derived,
    /// The stored source and native record range the work came from; a coordinate, not a permit.
    pub source: crate::services::tui_o::shadow::SourceId,
    pub range: std::ops::Range<u64>,
    pub origin:
        Option<crate::services::tui_prompt_dedupe::codex_verified::provenance::dormant::OwedOrigin>,
}

pub struct UnitDeriver {
    channel: u64,
    provider: ShadowProvider,
    seals: SealRegistry,
    cards: Option<ToolCardRun>,
    summary: Option<TurnSummaryRun>,
}

impl UnitDeriver {
    pub fn new(channel: u64, provider: ShadowProvider) -> Self {
        let seals = SealRegistry::default();
        Self {
            channel,
            provider,
            seals,
            cards: None,
            summary: None,
        }
    }

    /// Delivery actors opt in; identity and fork-boundary scratch derivation stays unchanged.
    pub fn with_tool_cards(mut self, inputs: ToolCardInputs) -> Self {
        self.cards = Some(ToolCardRun::new(inputs));
        self
    }

    pub fn with_turn_summary(mut self, inputs: SummaryInputs) -> Self {
        self.summary = Some(TurnSummaryRun::new(self.provider, inputs));
        self
    }

    /// Units in record order; a repeated key with the same plan (a fork copy) yields nothing.
    pub fn derive(&mut self, record: &CapturedRecord) -> Vec<Derived> {
        self.derive_with_version(record, crate::services::tui_o::shadow::IDENTITY_VERSION)
    }

    pub(crate) fn derive_with_version(
        &mut self,
        record: &CapturedRecord,
        identity_version: u32,
    ) -> Vec<Derived> {
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
        if let Some(summary) = &mut self.summary {
            summary.observe(&value);
        }
        let mut tools = if let Some(cards) = &mut self.cards {
            cards.observe(&value);
            cards.tools(self.provider, &value).into_iter()
        } else {
            Vec::new().into_iter()
        };
        let mut out = Vec::new();
        for fact in classify_with_version(self.provider, &value, identity_version) {
            match fact {
                RecordFact::Unit(native_key, kind, content) => {
                    let (channel_id, provider) = (self.channel, self.provider);
                    let unit_key = UnitKey {
                        channel_id,
                        provider,
                        native_key,
                        kind,
                    };
                    let meta = (kind == UnitKind::Tool).then(|| tools.next()).flatten();
                    let failed = match self.provider {
                        ShadowProvider::Claude => Some(matches!(content, UnitContent::Payload(_))),
                        ShadowProvider::Codex => value
                            .pointer("/payload/is_error")
                            .or_else(|| value.pointer("/payload/isError"))
                            .and_then(serde_json::Value::as_bool)
                            .filter(|failed| *failed)
                            .or_else(|| code_mode_exec_failed(&value)),
                    };
                    out.extend(self.unit(unit_key, content, meta, failed));
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
                boundary @ (RecordFact::Prompt(..)
                | RecordFact::TurnStart(..)
                | RecordFact::Idle(..)) => {
                    if let Some(cards) = &mut self.cards {
                        out.extend(cards.close());
                    }
                    if let Some(summary) = &mut self.summary {
                        match boundary {
                            RecordFact::Prompt(true, _) => summary.prompt(&value),
                            RecordFact::TurnStart(id) => summary.start(&value, id),
                            RecordFact::Idle(id) => {
                                out.extend(summary.close(&value, id.as_deref()))
                            }
                            _ => {}
                        }
                    }
                }
                RecordFact::Assistant => {
                    if let Some(summary) = &mut self.summary {
                        summary.assistant(&value);
                    }
                }
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

    /// Whether a unit with this native key was already derived here.
    pub fn sealed(&self, native_key: &str, kind: UnitKind) -> bool {
        let (channel_id, provider, native_key) = (self.channel, self.provider, native_key.into());
        self.seals.is_sealed(&UnitKey {
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

    /// Projection buffers must be empty before a fresh-incarnation rebuild drops this deriver.
    pub(super) fn has_pending_projection_probe(&self) -> bool {
        self.cards
            .as_ref()
            .is_some_and(|run| run.has_pending_projection_probe())
            || self
                .summary
                .as_ref()
                .is_some_and(|run| run.has_pending_projection_probe())
    }

    fn unit(
        &mut self,
        unit_key: UnitKey,
        content: UnitContent,
        meta: Option<ToolMeta>,
        failed: Option<bool>,
    ) -> Vec<Derived> {
        if self.cards.is_some()
            && unit_key.kind == UnitKind::Tool
            && unit_key.native_key.contains('#')
        {
            return vec![Derived::Blocked {
                reason: "native tool key contains reserved card separator".into(),
            }];
        }
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
        let mut out = self
            .summary
            .as_mut()
            .map(|summary| summary.first(&unit_key))
            .unwrap_or_default();
        // Card actors record membership now; identity-only derivation keeps panel dispositions.
        if unit_key.kind == UnitKind::Tool {
            if let (Some(cards), Some(meta)) = (&mut self.cards, meta) {
                out.extend(cards.tool(unit_key, meta));
                return out;
            }
            let reason = TOOL_CALL_PANEL.into();
            out.push(Derived::Excluded { unit_key, reason });
            return out;
        }
        // Keep the native identity and original seal above for replay and fork comparisons.
        if unit_key.kind == UnitKind::ToolResult {
            let reason = TOOL_RESULT_INTERNAL.into();
            let id = unit_key.native_key.clone();
            out.push(Derived::Excluded { unit_key, reason });
            if let Some(cards) = &mut self.cards {
                out.extend(match failed {
                    Some(failed) => cards.result(&id, failed),
                    None => cards.result_unknown(&id),
                });
            }
            return out;
        }
        if matches!(planned, UnitPlan::Pieces(_))
            && let Some(cards) = &mut self.cards
        {
            out.extend(cards.close());
        }
        let work: Vec<_> = match (planned, content) {
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
        };
        out.extend(work);
        out
    }
}

// Code-mode `exec` output opens with the harness status line, Codex's only recorded outcome.
fn code_mode_exec_failed(value: &serde_json::Value) -> Option<bool> {
    if value.pointer("/payload/type")?.as_str()? != "custom_tool_call_output" {
        return None;
    }
    match value
        .pointer("/payload/output/0/text")?
        .as_str()?
        .lines()
        .next()?
    {
        "Script completed" => Some(false),
        "Script failed" => Some(true),
        _ => None,
    }
}
