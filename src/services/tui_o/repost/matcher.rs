//! Attributing observed messages to one piece: its receipt, then its marker, then an observed
//! nonce, then an exact payload match nobody else may claim. Anything unclear is never absence.

use std::collections::{BTreeMap, BTreeSet};

use super::evidence::{EvidenceScope, MATCHER_VERSION};
use crate::services::tui_o::repost::identity::{marker, payload_sha256};
use crate::services::tui_o::repost::o_piece_delivery::{PieceKey, Receipt, ReceiptMethod};
use crate::services::tui_o::repost::send::RepostIds;

/// A message as Discord returned it. `nonce` is only what came back, never what was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObservedMessage {
    pub(crate) id: u64,
    pub(crate) channel_id: u64,
    pub(crate) author_id: u64,
    pub(crate) content: String,
    pub(crate) footers: Vec<String>,
    pub(crate) nonce: Option<String>,
}

/// Who else may own a message. `Unknown` is a failed lookup, never an empty index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttributionSnapshot {
    Known {
        /// Recorded receipts of the channel, by message id.
        receipts: BTreeMap<u64, PieceKey>,
        /// Other unsettled pieces with this piece's payload.
        same_payload: Vec<PieceKey>,
    },
    Unknown(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryKind {
    /// The observed nonce came back while only the original had been sent.
    OriginalRecovered,
    /// A re-post carrying the piece's marker.
    Reposted,
    /// The piece posted, but which of its sends did is not known.
    OriginUnknown,
}

/// A message of this piece and how it was told apart; F5 records `receipt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValidatedReceipt {
    pub(crate) receipt: Receipt,
    pub(crate) channel_id: u64,
    pub(crate) recovery: RecoveryKind,
    pub(crate) observed_nonce: Option<String>,
    pub(crate) observed_marker: Option<String>,
    pub(crate) content_sha256: String,
    pub(crate) matcher_version: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Attribution {
    /// New messages of this piece. Two or more distinct ids are a duplicate, not a resend reason.
    pub(crate) found: BTreeMap<u64, ValidatedReceipt>,
    /// Messages already recorded as this piece's receipts.
    pub(crate) recorded: BTreeSet<u64>,
    /// Exact matches another piece, or a lookup that failed, could also claim.
    pub(crate) unattributed: BTreeSet<u64>,
    /// The payload under a footer that is not this piece's intact marker.
    pub(crate) damaged: BTreeSet<u64>,
}

impl Attribution {
    /// Nothing seen could be this piece.
    pub(crate) fn is_clear(&self) -> bool {
        self.found.is_empty()
            && self.recorded.is_empty()
            && self.unattributed.is_empty()
            && self.damaged.is_empty()
    }

    /// Distinct ids known to be this piece's messages.
    pub(crate) fn distinct_ids(&self) -> usize {
        self.found
            .keys()
            .chain(&self.recorded)
            .collect::<BTreeSet<_>>()
            .len()
    }
}

/// The re-post footer ends with a space and the marker, so a longer marker never matches.
fn carries(footer: &str, marker: &str) -> bool {
    footer
        .strip_suffix(marker)
        .is_some_and(|rest| rest.ends_with(' '))
}

/// Adds what `seen` shows about the piece of `scope` to `into`; repeated ids count once.
pub(crate) fn match_observations<'m>(
    scope: &EvidenceScope,
    snapshot: &AttributionSnapshot,
    seen: impl IntoIterator<Item = &'m ObservedMessage>,
    into: &mut Attribution,
) {
    let own = marker(&scope.key);
    let nonce = RepostIds::for_piece(&own).map(|ids| ids.nonce().to_owned());
    let (receipts, rivals) = match snapshot {
        AttributionSnapshot::Known {
            receipts,
            same_payload,
        } => (Some(receipts), same_payload.as_slice()),
        AttributionSnapshot::Unknown(_) => (None, &[][..]),
    };
    let channel = scope.key.unit().channel_id;
    for message in seen {
        if message.channel_id != channel || message.author_id != scope.sender_id {
            continue;
        }
        match receipts.and_then(|receipts| receipts.get(&message.id)) {
            Some(key) if *key == scope.key => {
                into.recorded.insert(message.id);
                continue;
            }
            Some(_) => continue,
            None => {}
        }
        let content_sha256 = payload_sha256(&message.content);
        let exact = content_sha256 == scope.payload_sha256;
        let found = |method, recovery, slot| ValidatedReceipt {
            receipt: Receipt {
                key: scope.key.clone(),
                message_id: message.id,
                author_id: message.author_id,
                slot,
                method,
            },
            channel_id: message.channel_id,
            recovery,
            observed_nonce: message.nonce.clone(),
            observed_marker: (method == ReceiptMethod::Marker).then(|| own.clone()),
            content_sha256: content_sha256.clone(),
            matcher_version: MATCHER_VERSION,
        };
        // Both re-post slots share the marker and nonce, so neither names the slot that posted.
        let attributed = if message.footers.iter().any(|footer| carries(footer, &own)) {
            Some(found(ReceiptMethod::Marker, RecoveryKind::Reposted, None))
        } else if !message.footers.is_empty() {
            let elsewhere = rivals.iter().any(|rival| {
                let theirs = marker(rival);
                message
                    .footers
                    .iter()
                    .any(|footer| carries(footer, &theirs))
            });
            if exact && !elsewhere {
                into.damaged.insert(message.id);
            }
            None
        } else if message.nonce.is_some() {
            let (recovery, slot) = if scope.additional_sent() {
                (RecoveryKind::OriginUnknown, None)
            } else {
                (RecoveryKind::OriginalRecovered, Some(0))
            };
            (message.nonce == nonce).then(|| found(ReceiptMethod::Nonce, recovery, slot))
        } else if exact && receipts.is_some() && rivals.is_empty() {
            Some(found(
                ReceiptMethod::ExactMatch,
                RecoveryKind::OriginUnknown,
                None,
            ))
        } else {
            if exact {
                into.unattributed.insert(message.id);
            }
            None
        };
        if let Some(receipt) = attributed {
            into.found.entry(message.id).or_insert(receipt);
        }
    }
}
