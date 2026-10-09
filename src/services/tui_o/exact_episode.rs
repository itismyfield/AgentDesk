use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) const STRICT_NAMESPACE: &str = "herdr_exact_episode";
pub(crate) const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExactEpisodePin {
    pub episode: Uuid,
    pub owner: String,
    pub execution_nonce: String,
    pub turn_nonce: String,
    pub inflight_identity: String,
    pub born_generation: u64,
    pub channel_id: String,
    pub expected_author: String,
    pub source: Option<SourceIdentity>,
    pub context: FrozenSettlementContext,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceIdentity {
    pub incarnation: Uuid,
    pub opener: u64,
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TerminalSeal {
    pub source: SourceIdentity,
    pub terminal_identity: String,
    pub terminal_end: u64,
    pub capture_witness: Uuid,
    pub derive_witness: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FrozenSettlementContext {
    pub intake: Option<(i64, u64)>,
    pub dispatch: Option<(i64, u64)>,
    pub aliases: Vec<String>,
    pub required_effects: Vec<String>,
    pub policy_version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExactPieceRef {
    pub episode: Uuid,
    pub source: SourceIdentity,
    pub native_unit: String,
    pub kind: String,
    pub range: (u64, u64),
    pub plan_version: u32,
    pub plan_digest: String,
    pub piece_index: u32,
    pub piece_count: u32,
    pub payload_digest: String,
    pub obligation: Uuid,
    pub attempt: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExactTerminalManifest {
    pub source: SourceIdentity,
    pub seal: Option<TerminalSeal>,
    pub captured_through: u64,
    pub derived_through: u64,
    pub membership_digest: String,
    pub required: Vec<ExactPieceRef>,
    pub no_body_policy: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectReceipt {
    pub requested_channel: String,
    pub returned_channel: String,
    pub message_id: String,
    pub author: String,
    pub payload_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FrontierWitness {
    pub id: Uuid,
    pub source: SourceIdentity,
    pub range: (u64, u64),
    pub digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub(crate) enum EpisodeEvidence {
    Pin(ExactEpisodePin),
    Manifest(ExactTerminalManifest),
    SourceResolved(SourceIdentity),
    Obligation {
        piece: ExactPieceRef,
    },
    Attempt {
        piece: ExactPieceRef,
        frontier: FrontierWitness,
    },
    Transport {
        piece: ExactPieceRef,
        receipt: DirectReceipt,
    },
    Committed {
        piece: ExactPieceRef,
        frontier: FrontierWitness,
    },
    WholeFrontier {
        manifest: ExactTerminalManifest,
        pieces: Vec<ExactPieceRef>,
        frontier: FrontierWitness,
    },
    InputAttemptBegun {
        nonce: Uuid,
    },
    SubmissionClosed {
        generation: u64,
        basis: SubmissionBasis,
        policy_version: u32,
    },
    Settled {
        effects: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SubmissionBasis {
    NoAttempt,
    GateRefused { nonce: Uuid },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EpisodeMetadata {
    pub namespace: String,
    pub version: u32,
    pub episode: Uuid,
    pub record: Uuid,
    pub evidence: EpisodeEvidence,
}

impl EpisodeMetadata {
    pub fn new(episode: Uuid, record: Uuid, evidence: EpisodeEvidence) -> Self {
        Self {
            namespace: STRICT_NAMESPACE.into(),
            version: VERSION,
            episode,
            record,
            evidence,
        }
    }
    pub fn supported(&self) -> bool {
        self.namespace == STRICT_NAMESPACE
            && self.version == VERSION
            && !self.episode.is_nil()
            && !self.record.is_nil()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Authority {
    Pending,
    Body,
    Policy,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Settlement {
    Outstanding,
    Settled,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StrictResolution {
    authority: Authority,
    settlement: Settlement,
    pub evidence_refs: Vec<Uuid>,
}
impl StrictResolution {
    pub fn authority(&self) -> Authority {
        self.authority
    }
    pub fn settlement(&self) -> Settlement {
        self.settlement
    }
}

use super::exact_pg::ConsistentEpisodeSnapshot;

pub(crate) fn resolve_strict(
    episode: Uuid,
    snapshot: &ConsistentEpisodeSnapshot,
) -> StrictResolution {
    let records = snapshot.records();
    let pending = || StrictResolution {
        authority: Authority::Pending,
        settlement: Settlement::Outstanding,
        evidence_refs: Vec::new(),
    };
    if records.is_empty()
        || records.iter().enumerate().any(|(i, r)| {
            !r.supported()
                || r.episode != episode
                || records[..i].iter().any(|p| p.record == r.record)
        })
    {
        return pending();
    }
    let pins: Vec<_> = records
        .iter()
        .filter_map(|r| {
            if let EpisodeEvidence::Pin(p) = &r.evidence {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    let [pin] = pins.as_slice() else {
        return pending();
    };
    if pin.episode != episode
        || pin.owner.is_empty()
        || pin.execution_nonce.is_empty()
        || pin.turn_nonce.is_empty()
        || pin.born_generation == 0
        || pin.inflight_identity.is_empty()
        || pin.channel_id.is_empty()
        || pin.expected_author.is_empty()
        || pin
            .context
            .required_effects
            .iter()
            .enumerate()
            .any(|(i, e)| e.is_empty() || pin.context.required_effects[..i].contains(e))
        || pin.context.policy_version == 0
        || pin
            .context
            .intake
            .is_some_and(|(id, birth)| id <= 0 || birth == 0)
        || pin
            .context
            .dispatch
            .is_some_and(|(id, birth)| id <= 0 || birth == 0)
    {
        return pending();
    }
    let sources: Vec<_> = records
        .iter()
        .filter_map(|r| {
            if let EpisodeEvidence::SourceResolved(source) = &r.evidence {
                Some(source)
            } else {
                None
            }
        })
        .collect();
    let source = match (pin.source.as_ref(), sources.as_slice()) {
        (Some(pinned), []) => Some(pinned),
        (None, [resolved]) => Some(*resolved),
        (Some(pinned), [resolved]) if pinned == *resolved => Some(pinned),
        (None, []) => None,
        _ => return pending(),
    };
    let manifests: Vec<_> = records
        .iter()
        .filter_map(|r| {
            if let EpisodeEvidence::Manifest(m) = &r.evidence {
                Some(m)
            } else {
                None
            }
        })
        .collect();
    let pieces: Vec<_> = records
        .iter()
        .filter_map(|r| match &r.evidence {
            EpisodeEvidence::Obligation { piece } => Some(piece),
            _ => None,
        })
        .collect();
    let valid_manifest = |m: &ExactTerminalManifest| {
        source.is_some_and(|source| {
            m.source == *source && !source.incarnation.is_nil() && !source.digest.is_empty()
        }) && m.seal.as_ref().is_some_and(|seal| {
            seal.source == m.source
                && !seal.terminal_identity.is_empty()
                && !seal.capture_witness.is_nil()
                && !seal.derive_witness.is_nil()
                && m.source.opener < seal.terminal_end
                && m.captured_through == seal.terminal_end
                && m.derived_through == seal.terminal_end
        }) && !m.membership_digest.is_empty()
    };
    let no_body_debt = pieces.is_empty()
        && !records.iter().any(|r| {
            matches!(
                r.evidence,
                EpisodeEvidence::Attempt { .. }
                    | EpisodeEvidence::Transport { .. }
                    | EpisodeEvidence::Committed { .. }
                    | EpisodeEvidence::WholeFrontier { .. }
            )
        });
    let closures: Vec<_> = records
        .iter()
        .filter_map(|r| {
            if let EpisodeEvidence::SubmissionClosed {
                generation,
                basis,
                policy_version,
            } = &r.evidence
            {
                Some((*generation, basis, *policy_version))
            } else {
                None
            }
        })
        .collect();
    let attempts: Vec<_> = records
        .iter()
        .filter_map(|r| {
            if let EpisodeEvidence::InputAttemptBegun { nonce } = r.evidence {
                Some(nonce)
            } else {
                None
            }
        })
        .collect();
    let policy = no_body_debt
        && (match closures.as_slice() {
            [(generation, basis, version)]
                if *generation > 0 && *version == pin.context.policy_version =>
            {
                match basis {
                    SubmissionBasis::NoAttempt => attempts.is_empty(),
                    SubmissionBasis::GateRefused { nonce } => {
                        attempts.as_slice() == [*nonce] && !nonce.is_nil()
                    }
                }
            }
            _ => false,
        } || matches!(manifests.as_slice(), [m] if valid_manifest(m) && m.required.is_empty() && m.no_body_policy == Some(pin.context.policy_version)))
        && manifests
            .iter()
            .all(|m| valid_manifest(m) && m.required.is_empty());
    let body = match manifests.as_slice() {
        [m] if valid_manifest(m)
            && !m.required.is_empty()
            && closures.is_empty()
            && m.no_body_policy.is_none() =>
        {
            let membership = records.iter().all(|r| match &r.evidence {
                EpisodeEvidence::Obligation { piece }
                | EpisodeEvidence::Attempt { piece, .. }
                | EpisodeEvidence::Transport { piece, .. }
                | EpisodeEvidence::Committed { piece, .. } => m.required.contains(piece),
                _ => true,
            }) && pieces.len() == m.required.len()
                && m.required.iter().enumerate().all(|(i, p)| {
                    !m.required[..i].iter().any(|prior| {
                        prior.obligation == p.obligation
                            || (prior.native_unit == p.native_unit
                                && prior.piece_index == p.piece_index)
                    }) && m
                        .required
                        .iter()
                        .filter(|other| {
                            other.native_unit == p.native_unit
                                && other.plan_digest == p.plan_digest
                                && other.piece_count == p.piece_count
                        })
                        .count()
                        == p.piece_count as usize
                        && p.episode == episode
                        && Some(&p.source) == source
                        && !p.obligation.is_nil()
                        && !p.attempt.is_nil()
                        && !p.native_unit.is_empty()
                        && !p.kind.is_empty()
                        && p.range.0 < p.range.1
                        && p.range.0 >= m.source.opener
                        && p.range.1 <= m.captured_through
                        && p.plan_version == VERSION
                        && !p.plan_digest.is_empty()
                        && !p.payload_digest.is_empty()
                        && p.piece_count > 0
                        && p.piece_index < p.piece_count
                        && pieces.iter().filter(|x| **x == p).count() == 1
                        && exact_child(p, pin, records)
                });
            let whole: Vec<_> = records
                .iter()
                .filter_map(|r| {
                    if let EpisodeEvidence::WholeFrontier {
                        manifest,
                        pieces,
                        frontier,
                    } = &r.evidence
                    {
                        Some((manifest, pieces, frontier))
                    } else {
                        None
                    }
                })
                .collect();
            membership
                && matches!(whole.as_slice(), [(manifest, proofs, frontier)] if *manifest == *m && proofs.len() == m.required.len() && m.required.iter().all(|p| proofs.iter().filter(|x| *x == p).count() == 1) && valid_frontier(frontier, &m.source, (m.source.opener, m.captured_through)))
        }
        _ => false,
    };
    let authority = if body {
        Authority::Body
    } else if policy {
        Authority::Policy
    } else {
        return pending();
    };
    let settlements: Vec<_> = records
        .iter()
        .filter_map(|r| {
            if let EpisodeEvidence::Settled { effects } = &r.evidence {
                Some(effects)
            } else {
                None
            }
        })
        .collect();
    let settled = !settlements.is_empty()
        && settlements.iter().all(|effects| {
            effects.len() == pin.context.required_effects.len()
                && pin
                    .context
                    .required_effects
                    .iter()
                    .all(|e| effects.iter().filter(|x| *x == e).count() == 1)
        });
    StrictResolution {
        authority,
        settlement: if settled {
            Settlement::Settled
        } else {
            Settlement::Outstanding
        },
        evidence_refs: records.iter().map(|r| r.record).collect(),
    }
}

fn valid_frontier(f: &FrontierWitness, source: &SourceIdentity, range: (u64, u64)) -> bool {
    !f.id.is_nil() && &f.source == source && f.range == range && !f.digest.is_empty()
}
fn exact_child(p: &ExactPieceRef, pin: &ExactEpisodePin, records: &[EpisodeMetadata]) -> bool {
    let attempts: Vec<_> = records
        .iter()
        .filter_map(|r| {
            if let EpisodeEvidence::Attempt { piece, frontier } = &r.evidence {
                (piece.obligation == p.obligation).then_some((piece, frontier))
            } else {
                None
            }
        })
        .collect();
    let receipts: Vec<_> = records
        .iter()
        .filter_map(|r| {
            if let EpisodeEvidence::Transport { piece, receipt } = &r.evidence {
                (piece.obligation == p.obligation).then_some((piece, receipt))
            } else {
                None
            }
        })
        .collect();
    let commits: Vec<_> = records
        .iter()
        .filter_map(|r| {
            if let EpisodeEvidence::Committed { piece, frontier } = &r.evidence {
                (piece.obligation == p.obligation).then_some((piece, frontier))
            } else {
                None
            }
        })
        .collect();
    matches!((attempts.as_slice(), receipts.as_slice(), commits.as_slice()), ([(a, af)], [(t, receipt)], [(c, cf)]) if *a == p && *t == p && *c == p && af == cf && valid_frontier(af, &p.source, p.range) && receipt.requested_channel == pin.channel_id && receipt.requested_channel == receipt.returned_channel && !receipt.message_id.is_empty() && receipt.author == pin.expected_author && receipt.payload_digest == p.payload_digest)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DbTarget {
    pub id: i64,
    pub birth: u64,
    pub episode: Uuid,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalIntent {
    BodyDone,
    PolicySettle,
    Failed,
    Unknown,
    RetryAsNew,
    Delete,
    Reopen,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalDisposition {
    Applied,
    AlreadySatisfied,
    Unchanged,
    Deferred,
}

#[cfg(test)]
#[path = "exact_episode_tests.rs"]
pub(crate) mod tests;
